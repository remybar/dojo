use std::ops::Deref;

use async_graphql::dynamic::indexmap::IndexMap;
use async_graphql::dynamic::{
    Field, FieldFuture, FieldValue, InputValue, SubscriptionField, SubscriptionFieldFuture, TypeRef,
};
use async_graphql::{Name, Value};
use async_recursion::async_recursion;
use chrono::format;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::SqliteRow;
use sqlx::{Pool, Sqlite};
use tokio_stream::StreamExt;
use torii_core::simple_broker::SimpleBroker;
use torii_core::types::Entity;

use super::inputs::keys_input::keys_argument;
use super::{BasicObject, ResolvableObject, TypeMapping, ValueMapping};
use crate::constants::{
    DATETIME_FORMAT, ENTITY_NAMES, ENTITY_TABLE, ENTITY_TYPE_NAME, EVENT_ID_COLUMN, ID_COLUMN,
    INTERNAL_ENTITY_ID_KEY,
};
use crate::mapping::ENTITY_TYPE_MAPPING;
use crate::object::{resolve_many, resolve_one};
use crate::query::{type_mapping_query, value_mapping_from_row};
use crate::types::TypeData;
use crate::utils::extract;
pub struct EntityObject;

impl BasicObject for EntityObject {
    fn name(&self) -> (&str, &str) {
        ENTITY_NAMES
    }

    fn type_name(&self) -> &str {
        ENTITY_TYPE_NAME
    }

    fn type_mapping(&self) -> &TypeMapping {
        &ENTITY_TYPE_MAPPING
    }

    fn related_fields(&self) -> Option<Vec<Field>> {
        Some(vec![model_union_field()])
    }
}

impl ResolvableObject for EntityObject {
    fn resolvers(&self) -> Vec<Field> {
        let resolve_one = resolve_one(
            ENTITY_TABLE,
            ID_COLUMN,
            self.name().0,
            self.type_name(),
            self.type_mapping(),
        );

        let mut resolve_many = resolve_many(
            ENTITY_TABLE,
            EVENT_ID_COLUMN,
            self.name().1,
            self.type_name(),
            self.type_mapping(),
        );
        resolve_many = keys_argument(resolve_many);

        vec![resolve_one, resolve_many]
    }

    fn subscriptions(&self) -> Option<Vec<SubscriptionField>> {
        Some(vec![
            SubscriptionField::new("entityUpdated", TypeRef::named_nn(self.type_name()), |ctx| {
                SubscriptionFieldFuture::new(async move {
                    let id = match ctx.args.get("id") {
                        Some(id) => Some(id.string()?.to_string()),
                        None => None,
                    };
                    // if id is None, then subscribe to all entities
                    // if id is Some, then subscribe to only the entity with that id
                    Ok(SimpleBroker::<Entity>::subscribe().filter_map(move |entity: Entity| {
                        if id.is_none() || id == Some(entity.id.clone()) {
                            Some(Ok(Value::Object(EntityObject::value_mapping(entity))))
                        } else {
                            // id != entity.id , then don't send anything, still listening
                            None
                        }
                    }))
                })
            })
            .argument(InputValue::new("id", TypeRef::named(TypeRef::ID))),
        ])
    }
}

impl EntityObject {
    pub fn value_mapping(entity: Entity) -> ValueMapping {
        let keys: Vec<&str> = entity.keys.split('/').filter(|&k| !k.is_empty()).collect();
        IndexMap::from([
            (Name::new("id"), Value::from(entity.id)),
            (Name::new("keys"), Value::from(keys)),
            (Name::new("eventId"), Value::from(entity.event_id)),
            (
                Name::new("createdAt"),
                Value::from(entity.created_at.format(DATETIME_FORMAT).to_string()),
            ),
            (
                Name::new("updatedAt"),
                Value::from(entity.updated_at.format(DATETIME_FORMAT).to_string()),
            ),
            (
                Name::new("executedAt"),
                Value::from(entity.executed_at.format(DATETIME_FORMAT).to_string()),
            ),
        ])
    }
}

fn model_union_field() -> Field {
    Field::new("models", TypeRef::named_list("ModelUnion"), move |ctx| {
        FieldFuture::new(async move {
            match ctx.parent_value.try_to_value()? {
                Value::Object(indexmap) => {
                    let mut conn = ctx.data::<Pool<Sqlite>>()?.acquire().await?;

                    let entity_id = extract::<String>(indexmap, "id")?;
                    // fetch name from the models table
                    // using the model id (hashed model name)
                    let model_ids: Vec<(String, String)> = sqlx::query_as(
                        "SELECT id, name
                        FROM models
                        WHERE id IN (    
                            SELECT model_id
                            FROM entity_model
                            WHERE entity_id = ?
                        )",
                    )
                    .bind(&entity_id)
                    .fetch_all(&mut *conn)
                    .await?;

                    let mut results: Vec<FieldValue<'_>> = Vec::new();
                    for (id, name) in model_ids {
                        // the model id in the model mmeebrs table is the hashed model name (id)
                        let type_mapping = type_mapping_query(&mut conn, &id).await?;

                        // but the table name for the model data is the unhashed model name
                        let data: ValueMapping = match model_data_recursive_query(
                            &mut conn,
                            vec![name.clone()],
                            &entity_id,
                            None,
                            &type_mapping,
                        )
                        .await?
                        {
                            Value::Object(map) => map,
                            _ => unreachable!(),
                        };

                        results.push(FieldValue::with_type(FieldValue::owned_any(data), name));
                    }

                    Ok(Some(FieldValue::list(results)))
                }
                _ => Err("incorrect value, requires Value::Object".into()),
            }
        })
    })
}

// TODO: flatten query
#[async_recursion]
pub async fn model_data_recursive_query(
    conn: &mut PoolConnection<Sqlite>,
    path_array: Vec<String>,
    entity_id: &str,
    idx: Option<i64>,
    type_mapping: &TypeMapping,
) -> sqlx::Result<Value> {
    // For nested types, we need to remove prefix in path array
    let namespace = format!("{}_", path_array[0]);
    let table_name = &path_array.join("$").replace(&namespace, "");
    let mut query = format!("SELECT * FROM {} WHERE entity_id = '{}' ", table_name, entity_id);
    if let Some(idx) = idx {
        query.push_str(&format!("AND idx = {}", idx));
    }

    let rows = sqlx::query(&query).fetch_all(conn.as_mut()).await?;
    if rows.is_empty() {
        return Ok(Value::Null);
    }

    let value_mapping: Value;
    let mut nested_value_mappings = Vec::new();

    for (idx, row) in rows.iter().enumerate() {
        let mut nested_value_mapping = value_mapping_from_row(&row, type_mapping, true)?;

        for (field_name, type_data) in type_mapping {
            if let TypeData::Nested((_, nested_mapping)) = type_data {
                let mut nested_path = path_array.clone();
                nested_path.push(field_name.to_string());

                let nested_values = model_data_recursive_query(
                    conn,
                    nested_path,
                    entity_id,
                    if rows.len() > 1 { Some(idx as i64) } else { None },
                    nested_mapping,
                )
                .await?;
                nested_value_mapping.insert(Name::new(field_name), nested_values);
            } else if let TypeData::List(inner) = type_data {
                let mut nested_path = path_array.clone();
                nested_path.push(field_name.to_string());

                let data = match model_data_recursive_query(
                    conn,
                    nested_path,
                    entity_id,
                    // this might need to be changed to support 2d+ arrays
                    None,
                    &IndexMap::from([(Name::new("data"), *inner.clone())]),
                )
                .await?
                {
                    // map our list which uses a data field as a place holder
                    // for all elements to get the elemnt directly
                    Value::List(data) => data
                        .iter()
                        .map(|v| match v {
                            Value::Object(map) => map.get(&Name::new("data")).unwrap().clone(),
                            _ => unreachable!(),
                        })
                        .collect(),
                    _ => unreachable!(),
                };

                nested_value_mapping.insert(Name::new(field_name), data);
            } else if let TypeData::Union((_, types)) = type_data {
                let mut enum_union = Vec::new();
                for (type_ref, mapping) in types {
                    let mut nested_path = path_array.clone();
                    nested_path.push(field_name.to_string());
                    nested_path.push(type_ref.to_string().split("_").next().unwrap().to_string());

                    let mapping: &IndexMap<_, _> = match &mapping {
                        TypeData::Nested((_, mapping)) => mapping,
                        _ => unreachable!(),
                    };
                    
                    let data = if mapping.get(&Name::new("value")).is_some() {
                        let query = format!(
                            "SELECT external_{} FROM {} WHERE entity_id = '{}'",
                            field_name,
                            table_name,
                            entity_id,
                        );

                        let (value,): (String,) = sqlx::query_as(&query).fetch_one(conn.as_mut()).await?;
                        Value::Object(IndexMap::from([(Name::new("value"), Value::from(value))]))
                    } else {
                        model_data_recursive_query(
                            conn,
                            nested_path,
                            entity_id,
                            if rows.len() > 1 { Some(idx as i64) } else { None },
                            mapping,
                        )
                        .await?
                    };

                    enum_union.push(data);
                }

                println!("Enum Union: {:#?}", enum_union);

                nested_value_mapping.insert(Name::new(field_name), Value::List(enum_union));
            }
        }

        nested_value_mappings.push(Value::Object(nested_value_mapping));
    }

    if nested_value_mappings.len() > 1 {
        value_mapping = Value::List(nested_value_mappings);
    } else {
        value_mapping = nested_value_mappings.pop().unwrap();
    }

    Ok(value_mapping)
}

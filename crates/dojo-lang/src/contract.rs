use std::collections::HashMap;

use cairo_lang_defs::patcher::{PatchBuilder, RewriteNode};
use cairo_lang_defs::plugin::{
    DynGeneratedFileAuxData, PluginDiagnostic, PluginGeneratedFile, PluginResult,
};
use cairo_lang_diagnostics::Severity;
use cairo_lang_syntax::node::ast::{ArgClause, Expr, MaybeModuleBody, OptionArgListParenthesized};
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::helpers::QueryAttrs;
use cairo_lang_syntax::node::{ast, ids, Terminal, TypedStablePtr, TypedSyntaxNode};
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use dojo_types::system::Dependency;
use dojo_world::manifest::utils::compute_bytearray_hash;

use crate::plugin::{DojoAuxData, SystemAuxData, DOJO_CONTRACT_ATTR};
use crate::syntax::world_param::{self, WorldParamInjectionKind};
use crate::syntax::{self_param, utils as syntax_utils};
use crate::utils::is_name_valid;

const DOJO_INIT_FN: &str = "dojo_init";
const CONTRACT_NAMESPACE: &str = "namespace";

#[derive(Clone, Default)]
pub struct ContractParameters {
    namespace: Option<String>,
}

pub struct DojoContract {
    diagnostics: Vec<PluginDiagnostic>,
    dependencies: HashMap<smol_str::SmolStr, Dependency>,
}

impl DojoContract {
    pub fn from_module(
        db: &dyn SyntaxGroup,
        module_ast: &ast::ItemModule,
        package_id: String,
    ) -> PluginResult {
        let name = module_ast.name(db).text(db);

        let mut diagnostics = vec![];
        let parameters = get_parameters(db, module_ast, &mut diagnostics);

        let mut system = DojoContract { diagnostics, dependencies: HashMap::new() };

        let mut has_event = false;
        let mut has_storage = false;
        let mut has_init = false;

        let contract_namespace = match parameters.namespace {
            Some(x) => x.to_string(),
            None => package_id,
        };

        for (id, value) in [("name", &name.to_string()), ("namespace", &contract_namespace)] {
            if !is_name_valid(value) {
                return PluginResult {
                    code: None,
                    diagnostics: vec![PluginDiagnostic {
                        stable_ptr: module_ast.stable_ptr().0,
                        message: format!(
                            "The contract {id} '{value}' can only contain characters (a-z/A-Z), \
                             numbers (0-9) and underscore (_)"
                        ),
                        severity: Severity::Error,
                    }],
                    remove_original_item: false,
                };
            }
        }

        let contract_name_selector = compute_bytearray_hash(&name);
        let contract_namespace_selector = compute_bytearray_hash(&contract_namespace);

        if let MaybeModuleBody::Some(body) = module_ast.body(db) {
            let mut body_nodes: Vec<_> = body
                .items(db)
                .elements(db)
                .iter()
                .flat_map(|el| {
                    if let ast::ModuleItem::Enum(enum_ast) = el {
                        if enum_ast.name(db).text(db).to_string() == "Event" {
                            has_event = true;
                            return system.merge_event(db, enum_ast.clone());
                        }
                    } else if let ast::ModuleItem::Struct(struct_ast) = el {
                        if struct_ast.name(db).text(db).to_string() == "Storage" {
                            has_storage = true;
                            return system.merge_storage(db, struct_ast.clone());
                        }
                    } else if let ast::ModuleItem::Impl(impl_ast) = el {
                        // If an implementation is not targetting the ContractState,
                        // the auto injection of self and world is not applied.
                        let trait_path = impl_ast.trait_path(db).node.get_text(db);
                        if trait_path.contains("<ContractState>") {
                            return system.rewrite_impl(db, impl_ast.clone());
                        }
                    } else if let ast::ModuleItem::FreeFunction(fn_ast) = el {
                        let fn_decl = fn_ast.declaration(db);
                        let fn_name = fn_decl.name(db).text(db);

                        if fn_name == DOJO_INIT_FN {
                            has_init = true;
                            return system.handle_init_fn(db, fn_ast);
                        }
                    }

                    vec![RewriteNode::Copied(el.as_syntax_node())]
                })
                .collect();

            if !has_init {
                let node = RewriteNode::interpolate_patched(
                    "
                    #[starknet::interface]
                    trait IDojoInit<ContractState> {
                        fn $init_name$(self: @ContractState);
                    }

                    #[abi(embed_v0)]
                    impl IDojoInitImpl of IDojoInit<ContractState> {
                        fn $init_name$(self: @ContractState) {
                            assert(starknet::get_caller_address() == \
                     self.world().contract_address, 'Only world can init');
                        }
                    }
                ",
                    &UnorderedHashMap::from([(
                        "init_name".to_string(),
                        RewriteNode::Text(DOJO_INIT_FN.to_string()),
                    )]),
                );
                body_nodes.append(&mut vec![node]);
            }

            if !has_event {
                body_nodes.append(&mut system.create_event())
            }

            if !has_storage {
                body_nodes.append(&mut system.create_storage())
            }

            let mut builder = PatchBuilder::new(db, module_ast);
            builder.add_modified(RewriteNode::interpolate_patched(
                "
                #[starknet::contract]
                mod $name$ {
                    use dojo::world;
                    use dojo::world::IWorldDispatcher;
                    use dojo::world::IWorldDispatcherTrait;
                    use dojo::world::IWorldProvider;
                    use dojo::system::ISystem;

                    component!(path: dojo::components::upgradeable::upgradeable, storage: \
                 upgradeable, event: UpgradeableEvent);

                    #[abi(embed_v0)]
                    impl SystemImpl of ISystem<ContractState> {
                        fn name(self: @ContractState) -> ByteArray {
                            \"$name$\"
                        }
                        fn selector(self: @ContractState) -> felt252 {
                            $contract_name_selector$
                        }

                        fn namespace(self: @ContractState) -> ByteArray {
                            \"$contract_namespace$\"
                        }

                        fn namespace_selector(self: @ContractState) -> felt252 {
                            $contract_namespace_selector$
                        }
                    }

                    #[abi(embed_v0)]
                    impl WorldProviderImpl of IWorldProvider<ContractState> {
                        fn world(self: @ContractState) -> IWorldDispatcher {
                            self.world_dispatcher.read()
                        }
                    }

                    #[abi(embed_v0)]
                    impl UpgradableImpl = \
                 dojo::components::upgradeable::upgradeable::UpgradableImpl<ContractState>;

                    $body$
                }
                ",
                &UnorderedHashMap::from([
                    ("name".to_string(), RewriteNode::Text(name.to_string())),
                    (
                        "contract_name_selector".to_string(),
                        RewriteNode::Text(contract_name_selector.to_string()),
                    ),
                    ("body".to_string(), RewriteNode::new_modified(body_nodes)),
                    (
                        "contract_namespace".to_string(),
                        RewriteNode::Text(contract_namespace.clone()),
                    ),
                    (
                        "contract_namespace_selector".to_string(),
                        RewriteNode::Text(contract_namespace_selector.to_string()),
                    ),
                ]),
            ));

            let (code, code_mappings) = builder.build();

            return PluginResult {
                code: Some(PluginGeneratedFile {
                    name: name.clone(),
                    content: code,
                    aux_data: Some(DynGeneratedFileAuxData::new(DojoAuxData {
                        models: vec![],
                        systems: vec![SystemAuxData {
                            name,
                            namespace: contract_namespace.clone(),
                            dependencies: system.dependencies.values().cloned().collect(),
                        }],
                        events: vec![],
                    })),
                    code_mappings,
                }),
                diagnostics: system.diagnostics,
                remove_original_item: true,
            };
        }

        PluginResult::default()
    }

    fn handle_init_fn(
        &mut self,
        db: &dyn SyntaxGroup,
        fn_ast: &ast::FunctionWithBody,
    ) -> Vec<RewriteNode> {
        let fn_decl = fn_ast.declaration(db);
        let fn_name = fn_decl.name(db).text(db);

        let (params_str, was_world_injected) = self.rewrite_parameters(
            db,
            fn_decl.signature(db).parameters(db),
            fn_ast.stable_ptr().untyped(),
        );

        let mut world_read = "";
        if was_world_injected {
            world_read = "let world = self.world_dispatcher.read();";
        }

        let body = fn_ast.body(db).as_syntax_node().get_text(db);

        let node = RewriteNode::interpolate_patched(
            "
                #[starknet::interface]
                trait IDojoInit<ContractState> {
                    fn $name$($params_str$);
                }

                #[abi(embed_v0)]
                impl IDojoInitImpl of IDojoInit<ContractState> {
                    fn $name$($params_str$) {
                        $world_read$
                        assert(starknet::get_caller_address() == self.world().contract_address, \
             'Only world can init');
                        $body$
                    }
                }
            ",
            &UnorderedHashMap::from([
                ("name".to_string(), RewriteNode::Text(fn_name.to_string())),
                ("params_str".to_string(), RewriteNode::Text(params_str)),
                ("body".to_string(), RewriteNode::Text(body)),
                ("world_read".to_string(), RewriteNode::Text(world_read.to_string())),
            ]),
        );

        vec![node]
    }

    pub fn merge_event(
        &mut self,
        db: &dyn SyntaxGroup,
        enum_ast: ast::ItemEnum,
    ) -> Vec<RewriteNode> {
        let mut rewrite_nodes = vec![];

        let elements = enum_ast.variants(db).elements(db);

        let variants = elements.iter().map(|e| e.as_syntax_node().get_text(db)).collect::<Vec<_>>();
        let variants = variants.join(",\n");

        rewrite_nodes.push(RewriteNode::interpolate_patched(
            "
            #[event]
            #[derive(Drop, starknet::Event)]
            enum Event {
                UpgradeableEvent: dojo::components::upgradeable::upgradeable::Event,
                $variants$
            }
            ",
            &UnorderedHashMap::from([("variants".to_string(), RewriteNode::Text(variants))]),
        ));
        rewrite_nodes
    }

    pub fn create_event(&mut self) -> Vec<RewriteNode> {
        vec![RewriteNode::Text(
            "
            #[event]
            #[derive(Drop, starknet::Event)]
            enum Event {
                UpgradeableEvent: dojo::components::upgradeable::upgradeable::Event,
            }
            "
            .to_string(),
        )]
    }

    pub fn merge_storage(
        &mut self,
        db: &dyn SyntaxGroup,
        struct_ast: ast::ItemStruct,
    ) -> Vec<RewriteNode> {
        let mut rewrite_nodes = vec![];

        let elements = struct_ast.members(db).elements(db);

        let members = elements.iter().map(|e| e.as_syntax_node().get_text(db)).collect::<Vec<_>>();
        let members = members.join(",\n");

        rewrite_nodes.push(RewriteNode::interpolate_patched(
            "
            #[storage]
            struct Storage {
                world_dispatcher: IWorldDispatcher,
                #[substorage(v0)]
                upgradeable: dojo::components::upgradeable::upgradeable::Storage,
                $members$
            }
            ",
            &UnorderedHashMap::from([("members".to_string(), RewriteNode::Text(members))]),
        ));
        rewrite_nodes
    }

    pub fn create_storage(&mut self) -> Vec<RewriteNode> {
        vec![RewriteNode::Text(
            "
            #[storage]
            struct Storage {
                world_dispatcher: IWorldDispatcher,
                #[substorage(v0)]
                upgradeable: dojo::components::upgradeable::upgradeable::Storage,
            }
            "
            .to_string(),
        )]
    }

    /// Rewrites parameter list by:
    ///  * adding `self` parameter based on the `world` parameter mutability. If `world` is not
    ///    provided, a `View` is assumed.
    ///  * removing `world` if present as first parameter, as it will be read from the first
    ///    function statement.
    ///
    /// Reports an error in case of:
    ///  * `self` used explicitly,
    ///  * multiple world parameters,
    ///  * the `world` parameter is not the first parameter and named 'world'.
    ///
    /// Returns
    ///  * the list of parameters in a String.
    ///  * true if the world has to be injected (found as the first param).
    pub fn rewrite_parameters(
        &mut self,
        db: &dyn SyntaxGroup,
        param_list: ast::ParamList,
        fn_diagnostic_item: ids::SyntaxStablePtrId,
    ) -> (String, bool) {
        let is_self_used = self_param::check_parameter(db, &param_list);

        let world_injection = world_param::parse_world_injection(
            db,
            param_list.clone(),
            fn_diagnostic_item,
            &mut self.diagnostics,
        );

        if is_self_used && world_injection != WorldParamInjectionKind::None {
            self.diagnostics.push(PluginDiagnostic {
                stable_ptr: fn_diagnostic_item,
                message: "You cannot use `self` and `world` parameters together.".to_string(),
                severity: Severity::Error,
            });
        }

        let mut params = param_list
            .elements(db)
            .iter()
            .filter_map(|param| {
                let (name, _, param_type) = syntax_utils::get_parameter_info(db, param.clone());

                // If the param is `IWorldDispatcher`, we don't need to keep it in the param list
                // as it is flatten in the first statement.
                if world_param::is_world_param(&name, &param_type) {
                    None
                } else {
                    Some(param.as_syntax_node().get_text(db))
                }
            })
            .collect::<Vec<_>>();

        match world_injection {
            WorldParamInjectionKind::None => {
                if !is_self_used {
                    params.insert(0, "self: @ContractState".to_string());
                }
            }
            WorldParamInjectionKind::View => {
                params.insert(0, "self: @ContractState".to_string());
            }
            WorldParamInjectionKind::External => {
                params.insert(0, "ref self: ContractState".to_string());
            }
        }

        (params.join(", "), world_injection != WorldParamInjectionKind::None)
    }

    /// Rewrites function statements by adding the reading of `world` at first statement.
    pub fn rewrite_statements(
        &mut self,
        db: &dyn SyntaxGroup,
        statement_list: ast::StatementList,
    ) -> String {
        let mut statements = statement_list
            .elements(db)
            .iter()
            .map(|e| e.as_syntax_node().get_text(db))
            .collect::<Vec<_>>();

        statements.insert(0, "let world = self.world_dispatcher.read();\n".to_string());
        statements.join("")
    }

    /// Rewrites function declaration by:
    ///  * adding `self` parameter if missing,
    ///  * removing `world` if present as first parameter (self excluded),
    ///  * adding `let world = self.world_dispatcher.read();` statement at the beginning of the
    ///    function to restore the removed `world` parameter.
    ///  * if `has_generate_trait` is true, the implementation containing the function has the
    ///    #[generate_trait] attribute.
    pub fn rewrite_function(
        &mut self,
        db: &dyn SyntaxGroup,
        fn_ast: ast::FunctionWithBody,
        has_generate_trait: bool,
    ) -> Vec<RewriteNode> {
        let mut rewritten_fn = RewriteNode::from_ast(&fn_ast);

        let (params_str, was_world_injected) = self.rewrite_parameters(
            db,
            fn_ast.declaration(db).signature(db).parameters(db),
            fn_ast.stable_ptr().untyped(),
        );

        if has_generate_trait && was_world_injected {
            self.diagnostics.push(PluginDiagnostic {
                stable_ptr: fn_ast.stable_ptr().untyped(),
                message: "You cannot use `world` and `#[generate_trait]` together. Use `self` \
                          instead."
                    .to_string(),
                severity: Severity::Error,
            });
        }

        // We always rewrite the params as the self parameter is added based on the
        // world mutability.
        let rewritten_params = rewritten_fn
            .modify_child(db, ast::FunctionWithBody::INDEX_DECLARATION)
            .modify_child(db, ast::FunctionDeclaration::INDEX_SIGNATURE)
            .modify_child(db, ast::FunctionSignature::INDEX_PARAMETERS);
        rewritten_params.set_str(params_str);

        // If the world was injected, we also need to rewrite the statements of the function
        // to ensure the `world` injection is effective.
        if was_world_injected {
            let rewritten_statements = rewritten_fn
                .modify_child(db, ast::FunctionWithBody::INDEX_BODY)
                .modify_child(db, ast::ExprBlock::INDEX_STATEMENTS);

            rewritten_statements
                .set_str(self.rewrite_statements(db, fn_ast.body(db).statements(db)));
        }

        vec![rewritten_fn]
    }

    /// Rewrites all the functions of a Impl block.
    fn rewrite_impl(&mut self, db: &dyn SyntaxGroup, impl_ast: ast::ItemImpl) -> Vec<RewriteNode> {
        let generate_attrs = impl_ast.attributes(db).query_attr(db, "generate_trait");
        let has_generate_trait = !generate_attrs.is_empty();

        if let ast::MaybeImplBody::Some(body) = impl_ast.body(db) {
            let body_nodes: Vec<_> = body
                .items(db)
                .elements(db)
                .iter()
                .flat_map(|el| {
                    if let ast::ImplItem::Function(fn_ast) = el {
                        return self.rewrite_function(db, fn_ast.clone(), has_generate_trait);
                    }
                    vec![RewriteNode::Copied(el.as_syntax_node())]
                })
                .collect();

            let mut builder = PatchBuilder::new(db, &impl_ast);
            builder.add_modified(RewriteNode::interpolate_patched(
                "$body$",
                &UnorderedHashMap::from([(
                    "body".to_string(),
                    RewriteNode::new_modified(body_nodes),
                )]),
            ));

            let mut rewritten_impl = RewriteNode::from_ast(&impl_ast);
            let rewritten_items = rewritten_impl
                .modify_child(db, ast::ItemImpl::INDEX_BODY)
                .modify_child(db, ast::ImplBody::INDEX_ITEMS);

            let (code, _) = builder.build();

            rewritten_items.set_str(code);
            return vec![rewritten_impl];
        }

        vec![RewriteNode::Copied(impl_ast.as_syntax_node())]
    }
}

/// Get the contract namespace from the `Expr` parameter.
fn get_contract_namespace(
    db: &dyn SyntaxGroup,
    arg_value: Expr,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<String> {
    match arg_value {
        Expr::ShortString(ss) => Some(ss.string_value(db).unwrap()),
        Expr::String(s) => Some(s.string_value(db).unwrap()),
        _ => {
            diagnostics.push(PluginDiagnostic {
                message: format!(
                    "The argument '{}' of dojo::contract must be a string",
                    CONTRACT_NAMESPACE
                ),
                stable_ptr: arg_value.stable_ptr().untyped(),
                severity: Severity::Error,
            });
            Option::None
        }
    }
}

/// Get parameters of the dojo::contract attribute.
///
/// Parameters:
/// * db: The semantic database.
/// * module_ast: The AST of the contract module.
/// * diagnostics: vector of compiler diagnostics.
///
/// Returns:
/// * A [`ContractParameters`] object containing all the dojo::contract parameters with their
/// default values if not set in the code.
fn get_parameters(
    db: &dyn SyntaxGroup,
    module_ast: &ast::ItemModule,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> ContractParameters {
    let mut parameters = ContractParameters::default();
    let mut processed_args: HashMap<String, bool> = HashMap::new();

    if let OptionArgListParenthesized::ArgListParenthesized(arguments) =
        module_ast.attributes(db).query_attr(db, DOJO_CONTRACT_ATTR).first().unwrap().arguments(db)
    {
        arguments.arguments(db).elements(db).iter().for_each(|a| match a.arg_clause(db) {
            ArgClause::Named(x) => {
                let arg_name = x.name(db).text(db).to_string();
                let arg_value = x.value(db);

                if processed_args.contains_key(&arg_name) {
                    diagnostics.push(PluginDiagnostic {
                        message: format!("Too many '{}' attributes for dojo::contract", arg_name),
                        stable_ptr: module_ast.stable_ptr().untyped(),
                        severity: Severity::Error,
                    });
                } else {
                    processed_args.insert(arg_name.clone(), true);

                    match arg_name.as_str() {
                        CONTRACT_NAMESPACE => {
                            parameters.namespace =
                                get_contract_namespace(db, arg_value, diagnostics);
                        }
                        _ => {
                            diagnostics.push(PluginDiagnostic {
                                message: format!(
                                    "Unexpected argument '{}' for dojo::contract",
                                    arg_name
                                ),
                                stable_ptr: x.stable_ptr().untyped(),
                                severity: Severity::Warning,
                            });
                        }
                    }
                }
            }
            ArgClause::Unnamed(arg) => {
                let arg_name = arg.value(db).as_syntax_node().get_text(db);

                diagnostics.push(PluginDiagnostic {
                    message: format!("Unexpected argument '{}' for dojo::contract", arg_name),
                    stable_ptr: arg.stable_ptr().untyped(),
                    severity: Severity::Warning,
                });
            }
            ArgClause::FieldInitShorthand(x) => {
                diagnostics.push(PluginDiagnostic {
                    message: format!(
                        "Unexpected argument '{}' for dojo::contract",
                        x.name(db).name(db).text(db).to_string()
                    ),
                    stable_ptr: x.stable_ptr().untyped(),
                    severity: Severity::Warning,
                });
            }
        })
    }

    parameters
}

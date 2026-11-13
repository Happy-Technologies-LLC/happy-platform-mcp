//! Lane C: update sets, application scope, background and fix scripts,
//! workflows (13 tools, 6.2.0 parity).

mod bulk;
mod common;
mod defs;
mod js_text;
mod normalize;
mod pickers;
mod scripts;
mod workflow;

use std::sync::Arc;

use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::CredentialNeed;
use hpm_types::tool::{
    AccessClass, HandlerCompletion, Normalizer, OutputClass, ResultBody, RetryEligibility,
    SideEffectClass, ToolDescriptor, ToolHandler,
};
use serde_json::Value;

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};

type Run = for<'a> fn(&'a FamilyDeps, &'a RequestContext, Value) -> BoxFut<'a, HandlerCompletion>;

struct Handler {
    deps: FamilyDeps,
    run: Run,
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        (self.run)(&self.deps, ctx, args)
    }
}

/// Every lane C success body is verbatim 6.2.0 text.
fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::ToolResult { text, .. } | ResultBody::Message(text) => text.clone(),
        _ => String::new(),
    }
}

struct Spec {
    name: &'static str,
    description: &'static str,
    schema: &'static str,
    normalize: Normalizer,
    run: Run,
    access: AccessClass,
    output: OutputClass,
    side_effect: SideEffectClass,
    retry: RetryEligibility,
    credentials: CredentialNeed,
}

pub fn register(registry: &mut ToolRegistry, deps: &FamilyDeps) -> Result<(), RegistrationError> {
    use AccessClass as A;
    use CredentialNeed as C;
    use OutputClass as O;
    use RetryEligibility as R;
    use SideEffectClass as S;
    let specs = [
        Spec {
            name: "SN-Set-Update-Set",
            description: defs::SET_UPDATE_SET_DESC,
            schema: defs::SET_UPDATE_SET_SCHEMA,
            normalize: normalize::set_update_set,
            run: pickers::set_update_set,
            access: A::Write,
            output: O::Schema,
            side_effect: S::Local,
            retry: R::Idempotent,
            credentials: C::RestAndUiSession,
        },
        Spec {
            name: "SN-Get-Current-Update-Set",
            description: defs::GET_CURRENT_UPDATE_SET_DESC,
            schema: defs::GET_CURRENT_UPDATE_SET_SCHEMA,
            normalize: normalize::get_current_update_set,
            run: pickers::get_current_update_set,
            access: A::Read,
            output: O::Schema,
            side_effect: S::None,
            retry: R::Idempotent,
            credentials: C::RestAndUiSession,
        },
        Spec {
            name: "SN-List-Update-Sets",
            description: defs::LIST_UPDATE_SETS_DESC,
            schema: defs::LIST_UPDATE_SETS_SCHEMA,
            normalize: normalize::list_update_sets,
            run: pickers::list_update_sets,
            access: A::Read,
            output: O::Records,
            side_effect: S::None,
            retry: R::Idempotent,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Set-Current-Application",
            description: defs::SET_CURRENT_APPLICATION_DESC,
            schema: defs::SET_CURRENT_APPLICATION_SCHEMA,
            normalize: normalize::set_current_application,
            run: pickers::set_current_application,
            access: A::Write,
            output: O::Schema,
            side_effect: S::Local,
            retry: R::Idempotent,
            credentials: C::RestAndUiSession,
        },
        Spec {
            name: "SN-Execute-Background-Script",
            description: defs::EXECUTE_BACKGROUND_SCRIPT_DESC,
            schema: defs::EXECUTE_BACKGROUND_SCRIPT_SCHEMA,
            normalize: normalize::execute_background_script,
            run: scripts::execute_background_script,
            access: A::Execute,
            output: O::Opaque,
            side_effect: S::Remote,
            retry: R::Never,
            credentials: C::RestAndUiSession,
        },
        Spec {
            name: "SN-Create-Fix-Script",
            description: defs::CREATE_FIX_SCRIPT_DESC,
            schema: defs::CREATE_FIX_SCRIPT_SCHEMA,
            normalize: normalize::create_fix_script,
            run: scripts::create_fix_script,
            access: A::Write,
            output: O::None,
            side_effect: S::Local,
            retry: R::Idempotent,
            credentials: C::None,
        },
        Spec {
            name: "SN-Inspect-Update-Set",
            description: defs::INSPECT_UPDATE_SET_DESC,
            schema: defs::INSPECT_UPDATE_SET_SCHEMA,
            normalize: normalize::inspect_update_set,
            run: pickers::inspect_update_set,
            access: A::Read,
            output: O::Records,
            side_effect: S::None,
            retry: R::Idempotent,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Create-Workflow",
            description: defs::CREATE_WORKFLOW_DESC,
            schema: defs::CREATE_WORKFLOW_SCHEMA,
            normalize: normalize::create_workflow,
            run: workflow::create_workflow,
            access: A::Execute,
            output: O::Records,
            side_effect: S::Remote,
            retry: R::Never,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Create-Activity",
            description: defs::CREATE_ACTIVITY_DESC,
            schema: defs::CREATE_ACTIVITY_SCHEMA,
            normalize: normalize::create_activity,
            run: workflow::create_activity,
            access: A::Execute,
            output: O::Records,
            side_effect: S::Remote,
            retry: R::Never,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Create-Transition",
            description: defs::CREATE_TRANSITION_DESC,
            schema: defs::CREATE_TRANSITION_SCHEMA,
            normalize: normalize::create_transition,
            run: workflow::create_transition,
            access: A::Write,
            output: O::Records,
            side_effect: S::Remote,
            retry: R::Never,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Publish-Workflow",
            description: defs::PUBLISH_WORKFLOW_DESC,
            schema: defs::PUBLISH_WORKFLOW_SCHEMA,
            normalize: normalize::publish_workflow,
            run: workflow::publish_workflow,
            access: A::Execute,
            output: O::Records,
            side_effect: S::Remote,
            retry: R::BeforeSendOnly,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Move-Records-To-Update-Set",
            description: defs::MOVE_RECORDS_TO_UPDATE_SET_DESC,
            schema: defs::MOVE_RECORDS_TO_UPDATE_SET_SCHEMA,
            normalize: normalize::move_records_to_update_set,
            run: bulk::move_records_to_update_set,
            access: A::Write,
            output: O::Schema,
            side_effect: S::Remote,
            retry: R::BeforeSendOnly,
            credentials: C::Rest,
        },
        Spec {
            name: "SN-Clone-Update-Set",
            description: defs::CLONE_UPDATE_SET_DESC,
            schema: defs::CLONE_UPDATE_SET_SCHEMA,
            normalize: normalize::clone_update_set,
            run: bulk::clone_update_set,
            access: A::Write,
            output: O::Schema,
            side_effect: S::Remote,
            retry: R::Never,
            credentials: C::Rest,
        },
    ];
    for spec in specs {
        let handler = Arc::new(Handler {
            deps: deps.clone(),
            run: spec.run,
        });
        registry.register_tool(
            ToolDescriptor::new(
                spec.name,
                spec.description,
                spec.schema,
                spec.normalize,
                handler,
                render,
            )
            .access(spec.access)
            .output(spec.output)
            .side_effect(spec.side_effect)
            .retry(spec.retry)
            .needs_instance(true)
            .credentials(spec.credentials),
        )?;
    }
    Ok(())
}

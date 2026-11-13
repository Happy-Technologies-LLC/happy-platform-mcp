// Tool-behaviour scenarios: one or more per tool (all 60 in the 6.2.0 tools/list).
// Each scenario runs against a fresh JS stdio server + the loopback stub.
import { sysId } from '../lib/stub-sn.mjs';
import { FAKE_PASSWORD } from '../lib/world.mjs';

const U = id => sysId('arg', id);           // deterministic 32-hex ids for tool arguments
export const scenarios = [];

function add(tool, family, name, args, extra = {}) {
  scenarios.push({
    id: `${tool}/${name}`,
    tool, family,
    description: extra.description || `${tool} ${name}`,
    steps: extra.steps || [{ call: { name: tool, arguments: args, ...(extra.meta ? { _meta: extra.meta } : {}) } }],
    keyring: extra.keyring, stub: extra.stub, instances: extra.instances, env: extra.env, args: extra.argv, mode: extra.mode
  });
}

const json = (status, body, headers) => [status, body, headers];

const HISTORY_LINK = id => id ? `<A HREF="sys_script_execution_history.do?sys_id=${id}&amp;sysparm_view=">Script execution history and recovery available here</A><BR/>` : '';
function processorPage({ scope = 'global', lines = [], historyId = null } = {}) {
  const pre = lines.map(line => `${line}<BR/>`).join('');
  return `<HTML><BODY><HR/><PRE>${pre}</PRE><HR/>[0:00:00.004] Script completed in scope ${scope}: script<BR/>${HISTORY_LINK(historyId)}</BODY></HTML>`;
}
// Routes for the /sys.scripts.do protocol (session bootstrap, CSRF token, processor).
const SESSION_ID = 'GOLDENSESSION';
const CSRF = 'golden-csrf-token';
function processorStub(page, processorStatus = 200) {
  return ctx => {
    const { req } = ctx;
    const cookie = { 'set-cookie': `glide_session_store=${SESSION_ID}; Path=/; HttpOnly` };
    if (req.method === 'POST' && req.path === '/angular.do') return json(200, {}, cookie);
    if (req.method === 'GET' && req.path === '/api/now/ui/user/current_user') return json(200, { result: { user_name: 'golden.user' } }, cookie);
    if (req.method === 'GET' && req.path === '/api/now/table/sys_user_session') return json(200, { result: [{ csrf_token: CSRF, id: SESSION_ID }] });
    if (req.method === 'POST' && req.path === '/sys.scripts.do') {
      const body = page(ctx);
      return processorStatus === 200 ? [200, body ?? '', {}] : [processorStatus, 'forbidden', {}];
    }
    if (req.method === 'PUT' && req.path.startsWith('/api/now/table/sys_trigger/')) {
      ctx.state.triggerMarker = /gs\.info\('([^']+):END'/.exec(req.body?.script || '')?.[1] ?? ctx.state.triggerMarker;
      return json(200, { result: { sys_id: req.path.split('/').pop() } });
    }
    if (req.method === 'GET' && req.path === '/api/now/table/syslog' && ctx.state.triggerMarker) {
      return json(200, { result: [{ message: `${ctx.state.triggerMarker}:END${JSON.stringify({ ok: true })}`, sys_created_on: '2099-01-01 00:00:00' }] });
    }
    if (req.method === 'GET' && req.path === '/api/now/table/sys_scope') return json(200, { result: [{ sys_id: sysId('scope', 1), scope: 'x_golden_app1', name: 'Golden App 1', active: 'true' }] });
    if (req.method === 'GET' && req.path === '/api/now/table/sys_script_execution_history') return json(200, { result: [{ rollback_context: { value: sysId('rollback', 1), link: '{{x}}' } }] });
    return null;
  };
}
const errorBody = (message, detail = 'golden') => ({ error: { message, detail }, status: 'failure' });

// ---- Generic table CRUD -------------------------------------------------
add('SN-Query-Table', 'crud', 'basic', { table_name: 'incident', query: 'active=true^priority=1', limit: 2, fields: 'number,short_description,state', order_by: '-sys_created_on' });
add('SN-Query-Table', 'crud', 'offset-defaults', { table_name: 'sys_user' });
add('SN-Query-Table', 'crud', 'http-403', { table_name: 'incident', query: 'active=true' }, {
  stub: ({ req }) => req.path.startsWith('/api/now/table/') ? json(403, errorBody('User Not Authorized', 'ACL exception')) : null
});
add('SN-Query-Table', 'crud', 'invalid-table-arg', { table_name: 'bad table!' });
add('SN-Create-Record', 'crud', 'basic', { table_name: 'incident', data: { short_description: 'golden create', urgency: '2' } });
add('SN-Get-Record', 'crud', 'basic', { table_name: 'incident', sys_id: U(1), fields: 'number,short_description' });
add('SN-Get-Record', 'crud', 'http-404', { table_name: 'incident', sys_id: U(2) }, {
  stub: ({ req }) => req.method === 'GET' ? json(404, errorBody('No Record found', 'Record doesn\'t exist or ACL restricts the record retrieval')) : null
});
add('SN-Update-Record', 'crud', 'basic', { table_name: 'incident', sys_id: U(1), data: { state: '2', work_notes: 'golden update' } });
add('SN-Get-Table-Schema', 'schema', 'basic', { table_name: 'incident' });
add('SN-List-Available-Tables', 'schema', 'basic', {});
add('SN-List-Available-Tables', 'schema', 'category-itsm', { category: 'itsm' });
add('SN-Discover-Table-Schema', 'schema', 'basic', { table_name: 'incident' });
add('SN-Discover-Table-Schema', 'schema', 'all-includes', { table_name: 'incident', include_type_codes: true, include_choice_tables: true, include_relationships: true, include_business_rules: true, include_ui_policies: true, include_field_constraints: true });
add('SN-Explain-Field', 'schema', 'basic', { table: 'incident', field: 'state', include_examples: true });

// ---- Typed list/get tools -------------------------------------------------
add('SN-List-Incidents', 'itsm', 'filters', { state: '1', priority: '2', query: 'category=software', limit: 5, offset: 10, fields: 'number,short_description', order_by: '-sys_updated_on' });
add('SN-Create-Incident', 'itsm', 'basic', { short_description: 'golden incident', description: 'created by golden capture', urgency: '2', impact: '2', category: 'software' });
add('SN-Get-Incident', 'itsm', 'basic', { sys_id: U(1) });
add('SN-List-SysUsers', 'itsm', 'basic', { query: 'active=true', limit: 3 });
add('SN-List-CmdbCis', 'itsm', 'basic', { query: 'operational_status=1', limit: 3 });
add('SN-List-SysUserGroups', 'itsm', 'basic', { limit: 3 });
add('SN-List-ChangeRequests', 'itsm', 'basic', { state: '-5', type: 'normal', limit: 3 });
add('SN-List-Problems', 'itsm', 'basic', { query: 'active=true', limit: 3 });
add('SN-Natural-Language-Search', 'itsm', 'basic', { query: 'high priority incidents assigned to me', limit: 5 });
add('SN-Natural-Language-Search', 'itsm', 'show-patterns', { query: 'P1 incidents', show_patterns: true });

// ---- Update sets / application scope -------------------------------------
add('SN-Set-Update-Set', 'update-sets', 'verified', { update_set_sys_id: U(10) });
add('SN-Set-Update-Set', 'update-sets', 'bad-sys-id', { update_set_sys_id: 'not-a-sys-id' });
add('SN-Set-Update-Set', 'update-sets', 'picker-fails-fix-script', { update_set_sys_id: U(10) }, {
  stub: ({ req }) => req.path.startsWith('/api/now/ui/concoursepicker') ? json(500, errorBody('picker unavailable')) : null
});
add('SN-Get-Current-Update-Set', 'update-sets', 'basic', {});
add('SN-List-Update-Sets', 'update-sets', 'basic', { query: 'state=in progress', limit: 3, fields: 'name,state' });
add('SN-Set-Current-Application', 'update-sets', 'verified', { app_sys_id: U(11) });
add('SN-Set-Current-Application', 'update-sets', 'global', { app_sys_id: 'global' });
add('SN-Set-Current-Application', 'update-sets', 'bad-sys-id', { app_sys_id: 'nope' });
add('SN-Inspect-Update-Set', 'update-sets', 'by-sys-id', { update_set: U(10), show_components: true, show_dependencies: true });
add('SN-Inspect-Update-Set', 'update-sets', 'by-name', { update_set: 'Golden Update Set 1' });
add('SN-Clone-Update-Set', 'update-sets', 'basic', { source_update_set_id: U(10), new_name: 'Golden Clone' });
add('SN-Move-Records-To-Update-Set', 'update-sets', 'by-ids', { update_set_id: U(10), record_sys_ids: [U(20), U(21)] });
add('SN-Move-Records-To-Update-Set', 'update-sets', 'by-time-range', { update_set_id: U(10), source_update_set: 'Default', time_range: { start: '2026-01-01 00:00:00', end: '2026-01-01 01:00:00' } });

// ---- Scripts --------------------------------------------------------------
add('SN-Create-Fix-Script', 'scripts', 'basic', { script_name: 'golden_fix', script_content: 'gs.info("golden fix script");', description: 'Golden fix script', auto_delete: true });
add('SN-Execute-Background-Script', 'scripts', 'empty-script', { script: '   ' });
add('SN-Execute-Background-Script', 'scripts', 'processor-success', { script: 'gs.print("golden background");', description: 'golden processor run' }, {
  stub: processorStub(({ state }) => processorPage({ lines: ['*** Script: golden background'], historyId: sysId('hist', 1) }))
});
add('SN-Execute-Background-Script', 'scripts', 'processor-script-error', { script: 'var x = ;' }, {
  stub: processorStub(() => processorPage({ lines: ['Script compilation error: Unexpected token ;'], historyId: sysId('hist', 2) }))
});
add('SN-Execute-Background-Script', 'scripts', 'processor-unauthorized-trigger-fallback', { script: 'gs.print("fallback");' }, {
  stub: processorStub(() => null, 403)
});
add('SN-Execute-Background-Script', 'scripts', 'scoped-run', { script: 'gs.print("scoped");', scope: 'x_golden_app1' }, {
  stub: processorStub(() => processorPage({ scope: 'x_golden_app1', lines: ['x_golden_app1: scoped'], historyId: null }))
});
add('SN-Execute-Background-Script', 'scripts', 'bad-scope', { script: 'gs.print(1);', scope: 'Not A Scope!' });

// ---- Incident / change / problem lifecycle --------------------------------
add('SN-Add-Comment', 'itsm-ops', 'basic', { incident_number: 'INC0010001', comment: 'golden customer-visible comment' });
add('SN-Add-Work-Notes', 'itsm-ops', 'basic', { incident_number: 'INC0010001', work_notes: 'golden internal work note' });
add('SN-Add-Work-Notes', 'itsm-ops', 'incident-not-found', { incident_number: 'INC9999999', work_notes: 'x' }, {
  stub: ({ req }) => req.method === 'GET' && req.path === '/api/now/table/incident' ? json(200, { result: [] }) : null
});
add('SN-Assign-Incident', 'itsm-ops', 'user-and-group', { incident_number: 'INC0010001', assigned_to: 'golden.user1', assignment_group: 'Golden Group 1' });
add('SN-Resolve-Incident', 'itsm-ops', 'basic', { incident_number: 'INC0010001', resolution_notes: 'golden resolution', resolution_code: 'Solution provided' });
add('SN-Close-Incident', 'itsm-ops', 'basic', { incident_number: 'INC0010001', close_notes: 'golden close', close_code: 'Solved (Permanently)' });
add('SN-Add-Change-Comment', 'itsm-ops', 'basic', { change_number: 'CHG0030001', comment: 'golden change comment' });
add('SN-Assign-Change', 'itsm-ops', 'basic', { change_number: 'CHG0030001', assigned_to: 'golden.user1', assignment_group: 'Golden Group 1' });
add('SN-Approve-Change', 'itsm-ops', 'basic', { change_number: 'CHG0030001', approval_comments: 'golden approval' });
add('SN-Add-Problem-Comment', 'itsm-ops', 'basic', { problem_number: 'PRB0040001', comment: 'golden problem comment' });
add('SN-Close-Problem', 'itsm-ops', 'basic', { problem_number: 'PRB0040001', resolution_notes: 'golden problem resolution', resolution_code: 'Fix applied' });

// ---- Batch ------------------------------------------------------------------
add('SN-Batch-Create', 'batch', 'basic-with-refs', { operations: [
  { table: 'incident', data: { short_description: 'golden batch parent' }, save_as: 'parent' },
  { table: 'incident', data: { short_description: 'golden batch child', parent_incident: '${parent}' } }
], transaction: true, progress: true });
add('SN-Batch-Create', 'batch', 'failure-rollback', { operations: [
  { table: 'incident', data: { short_description: 'golden ok' } },
  { table: 'problem', data: { short_description: 'golden fails' } }
], transaction: true }, {
  stub: ({ req }) => req.method === 'POST' && req.path === '/api/now/table/problem' ? json(403, errorBody('Insert Operation Failed: ACL')) : null
});
add('SN-Batch-Update', 'batch', 'basic', { updates: [
  { table: 'incident', sys_id: U(1), data: { state: '2' } },
  { table: 'incident', sys_id: U(2), data: { state: '3' } }
], stop_on_error: false, progress: true });
add('SN-Batch-Update', 'batch', 'stop-on-error', { updates: [
  { table: 'incident', sys_id: U(1), data: { state: '2' } },
  { table: 'incident', sys_id: U(3), data: { state: '3' } },
  { table: 'incident', sys_id: U(4), data: { state: '4' } }
], stop_on_error: true }, {
  stub: ({ req }) => req.method === 'PATCH' || req.method === 'PUT' ? (req.path.endsWith(U(3)) ? json(404, errorBody('No Record found')) : null) : null
});

// ---- Catalog -----------------------------------------------------------------
add('SN-Catalog-Get-Categories', 'catalog', 'basic', { query: 'hardware', limit: 5 });
add('SN-Catalog-Search-Items', 'catalog', 'keyword', { keyword: 'laptop', include_producers: true, active: true, limit: 5 });
add('SN-Catalog-Get-Item', 'catalog', 'basic', { sys_id: U(30) });
add('SN-Catalog-Submit', 'catalog', 'basic', { sys_id: U(30), variables: { model: 'golden-laptop', justification: 'golden' }, quantity: 2, requested_for: U(31) }, {
  stub: ({ req }) => req.method === 'POST' && req.path.endsWith('/order_now')
    ? json(200, { result: { sys_id: sysId('request', 1), number: 'REQ0010001', request_number: 'REQ0010001', request_id: sysId('request', 1), table: 'sc_request' } })
    : null
});
add('SN-Catalog-Submit', 'catalog', 'http-400', { sys_id: U(30), variables: {} }, {
  stub: ({ req }) => req.method === 'POST' && req.path.endsWith('/order_now') ? json(400, errorBody('Mandatory variables missing')) : null
});
add('SN-Validate-Configuration', 'catalog', 'basic', { catalog_item: U(30), checks: { variables: true, ui_policies: true, business_rules: true } });

// ---- Workflow ------------------------------------------------------------------
add('SN-Create-Workflow', 'workflow', 'two-activities', { name: 'Golden Workflow', table: 'incident', description: 'golden', condition: 'active=true',
  activities: [{ name: 'Start', script: 'answer = true;' }, { name: 'Finish', script: 'gs.info("done");' }],
  transitions: [{ from: 0, to: 1, condition: 'true' }], publish: true, progress: true });
add('SN-Create-Activity', 'workflow', 'basic', { workflow_version_sys_id: U(40), name: 'Golden Activity', script: 'answer = true;', x: 100, y: 200 });
add('SN-Create-Transition', 'workflow', 'basic', { from_activity_sys_id: U(41), to_activity_sys_id: U(42), condition_script: 'answer = true;', order: 1 });
add('SN-Publish-Workflow', 'workflow', 'basic', { version_sys_id: U(40), start_activity_sys_id: U(41) });

// ---- ATF (sn_cicd) --------------------------------------------------------------
const ATF_PROGRESS = sysId('atf-progress', 1);
const ATF_RESULT = sysId('atf-result', 1);
const atfRoutes = ({ req, state }) => {
  if (req.method === 'POST' && (req.path === '/api/sn_cicd/tests/run_test' || req.path === '/api/sn_cicd/testsuite/run')) {
    return json(200, { result: { status: '0', links: { progress: { id: ATF_PROGRESS, url: `/api/sn_cicd/progress/${ATF_PROGRESS}` } } } });
  }
  if (req.path === `/api/sn_cicd/progress/${ATF_PROGRESS}`) {
    return json(200, { result: { status: '2', percent_complete: 100, status_label: 'Successful', status_message: 'Done', links: { results: { id: ATF_RESULT } } } });
  }
  if (req.path === `/api/sn_cicd/tests/test/results/${ATF_RESULT}`) {
    return json(200, { result: { test_name: 'Golden test', test_status: 'success', output: 'all good' } });
  }
  if (req.path === `/api/sn_cicd/tests/test/results/${ATF_RESULT}/logs`) {
    return json(200, { result: { logs: ['INFO step 1 ok', 'ERROR step 2 golden'] } });
  }
  if (req.path === `/api/sn_cicd/testsuite/results/${ATF_RESULT}`) {
    return json(200, { result: { test_suite_name: 'Golden Suite', test_suite_status: 'success', rolledup_test_success_count: 2, rolledup_test_failure_count: 0, rolledup_test_error_count: 0, rolledup_test_skip_count: 0 } });
  }
  return null;
};
add('SN-Run-ATF-Test', 'atf', 'by-sys-id-wait', { test_sys_id: U(50), run_in_cloud: false, poll_interval_seconds: 1, timeout_seconds: 10 }, { stub: atfRoutes });
add('SN-Run-ATF-Test', 'atf', 'by-name-no-wait', { test_name: 'Golden test', wait: false }, { stub: ctx => ctx.req.path === '/api/now/table/sys_atf_test' ? json(200, { result: [{ sys_id: U(50), name: 'Golden test' }] }) : atfRoutes(ctx) });
add('SN-Run-ATF-Test', 'atf', 'no-selector', {});
add('SN-Run-ATF-Suite', 'atf', 'by-sys-id-wait', { test_suite_sys_id: U(51), browser_name: 'chrome', poll_interval_seconds: 1, timeout_seconds: 10 }, { stub: atfRoutes });
add('SN-Get-CICD-Progress', 'atf', 'single-read', { progress_id: ATF_PROGRESS }, { stub: atfRoutes });
add('SN-Get-CICD-Progress', 'atf', 'http-403', { progress_id: ATF_PROGRESS }, { stub: ({ req }) => req.path.startsWith('/api/sn_cicd') ? json(403, errorBody('User is not authorized')) : null });
add('SN-Get-ATF-Results', 'atf', 'test', { result_id: ATF_RESULT, result_type: 'test' }, { stub: atfRoutes });
add('SN-Get-ATF-Results', 'atf', 'suite', { result_id: ATF_RESULT, result_type: 'suite' }, { stub: atfRoutes });
add('SN-Get-ATF-Results', 'atf', 'bad-result-type', { result_id: ATF_RESULT, result_type: 'all' });
add('SN-Get-ATF-Test-Logs', 'atf', 'pattern-limit', { result_id: ATF_RESULT, pattern: 'error|warn', limit: 20 }, { stub: atfRoutes });
add('SN-Get-ATF-Test-Logs', 'atf', 'bad-pattern', { result_id: ATF_RESULT, pattern: '(' });

// ---- Instances (registry mode) ------------------------------------------------
const registry = (world, extra = []) => ({
  version: 1,
  instances: [
    { name: 'dev', url: world.stub.origin, username: 'golden.user', password: FAKE_PASSWORD, description: 'Golden dev', default: true },
    { name: 'prod', url: `http://localhost:${world.stub.port}`, username: 'golden.user', password: FAKE_PASSWORD, description: 'Golden prod' },
    ...extra
  ]
});
add('SN-Get-Current-Instance', 'instances', 'registry-default', {}, { instances: registry, mode: 'registry-multi-instance' });
add('SN-Set-Instance', 'instances', 'list', {}, { instances: registry, mode: 'registry-multi-instance' });
add('SN-Set-Instance', 'instances', 'switch-then-current', { instance_name: 'prod' }, {
  instances: registry, mode: 'registry-multi-instance',
  steps: [
    { call: { name: 'SN-Set-Instance', arguments: { instance_name: 'prod' } } },
    { call: { name: 'SN-Get-Current-Instance', arguments: {} } },
    { call: { name: 'SN-Query-Table', arguments: { table_name: 'incident', limit: 1 } } }
  ]
});
add('SN-Set-Instance', 'instances', 'unknown-instance', { instance_name: 'nope' }, { instances: registry, mode: 'registry-multi-instance' });
add('SN-Query-Table', 'instances', 'per-call-instance-override', { table_name: 'incident', limit: 1, instance: 'prod' }, { instances: registry, mode: 'registry-multi-instance' });
add('SN-Query-Table', 'instances', 'unknown-instance-override', { table_name: 'incident', limit: 1, instance: 'nope' }, { instances: registry, mode: 'registry-multi-instance' });
add('SN-Register-Instance', 'instances', 'basic-with-stored-credential', { name: 'staging', url: 'https://staging.example.invalid', username: 'golden.user', description: 'Golden staging', makeDefault: false }, {
  instances: world => ({ version: 1, instances: [{ name: 'dev', url: world.stub.origin, username: 'golden.user', credentialRef: 'keychain:instance/dev/password', description: 'Golden dev', default: true }] }),
  mode: 'registry-credential-refs',
  keyring: { 'keychain:instance/dev/password': FAKE_PASSWORD, 'keychain:instance/staging/password': FAKE_PASSWORD },
  steps: [
    { call: { name: 'SN-Register-Instance', arguments: { name: 'staging', url: 'https://staging.example.invalid', username: 'golden.user', description: 'Golden staging', makeDefault: false } } },
    { call: { name: 'SN-Set-Instance', arguments: {} } }
  ]
});
add('SN-Register-Instance', 'instances', 'secret-field-rejected', { name: 'staging', url: 'https://staging.example.invalid', username: 'golden.user', password: 'must-not-be-accepted' }, { instances: registry, mode: 'registry-multi-instance' });
add('SN-Register-Instance', 'instances', 'insecure-url-rejected', { name: 'bad', url: 'http://staging.example.invalid', username: 'golden.user' }, { instances: registry, mode: 'registry-multi-instance' });

// ---- Docs (served from the loopback raw-GitHub stub) ----------------------------
add('SN-Docs-Families', 'docs', 'basic', {});
add('SN-Docs-Status', 'docs', 'empty-cache', {});
add('SN-Docs-Sync', 'docs', 'index-disabled-default', { family: 'australia' }, {
  steps: [
    { call: { name: 'SN-Docs-Sync', arguments: { family: 'australia' } }, timeoutMs: 120000 },
    { call: { name: 'SN-Docs-Status', arguments: {} } },
    { call: { name: 'SN-Docs-Search', arguments: { query: 'flow subflows', family: 'australia', limit: 3 } } },
    { call: { name: 'SN-Docs-Get', arguments: { family: 'australia', path: 'platform/flow.md' } } }
  ]
});
add('SN-Docs-Sync', 'docs', 'local-index-enabled', { family: 'australia' }, {
  env: { HAPPY_DOCS_ENABLE_LOCAL_INDEX: 'true' },
  steps: [
    { call: { name: 'SN-Docs-Sync', arguments: { family: 'australia' } }, timeoutMs: 120000 },
    { call: { name: 'SN-Docs-Status', arguments: {} } },
    { call: { name: 'SN-Docs-Search', arguments: { query: 'flow subflows', family: 'australia', limit: 3 } } }
  ]
});
add('SN-Docs-Search', 'docs', 'no-cache', { query: 'flow' });
add('SN-Docs-Get', 'docs', 'path-traversal-rejected', { family: 'australia', path: '../../etc/passwd' });

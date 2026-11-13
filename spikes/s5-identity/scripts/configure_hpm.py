#!/usr/bin/env python3
"""Create/refresh the `entra` identity provider (broker link) and role mappers in realm hpm.

UPSTREAM=fake  (default) -> the fake-entra Keycloak realm from docker-compose.
UPSTREAM=entra           -> a real Entra tenant. Needs ENTRA_TENANT_ID, ENTRA_CLIENT_ID and the client
                            secret in ENTRA_CLIENT_SECRET or the macOS keychain item
                            `security add-generic-password -s hpm-s5-entra -a client-secret -w`.
Idempotent: deletes and recreates the IdP so the config always matches the environment.
"""
from __future__ import annotations

import os
import subprocess
import sys

from lib import BROKER_SECRET, UP, UP_INTERNAL, UP_REALM, HPM_INTERNAL, HPM_REALM, hpm_admin, upstream_admin, wait_ready

# upstream app role value -> hpm realm role
ROLE_MAP = {"hpm.user": "mcp-user", "hpm.developer": "mcp-developer", "hpm.admin": "mcp-admin", "hpm.auditor": "mcp-auditor"}
ALIAS = "entra"


def keychain_secret() -> str:
    if os.environ.get("ENTRA_CLIENT_SECRET"):
        return os.environ["ENTRA_CLIENT_SECRET"]
    out = subprocess.run(["security", "find-generic-password", "-s", "hpm-s5-entra", "-a", "client-secret", "-w"],
                         capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit("no ENTRA_CLIENT_SECRET and no keychain item hpm-s5-entra/client-secret")
    return out.stdout.strip()


def upstream_config(kind: str) -> dict:
    """The only place the upstream IdP differs. Everything else is identical for fake and real Entra."""
    if kind == "entra":
        tid, cid = os.environ["ENTRA_TENANT_ID"], os.environ["ENTRA_CLIENT_ID"]
        base = f"https://login.microsoftonline.com/{tid}"
        return {
            "clientId": cid, "clientSecret": keychain_secret(),
            "clientAuthMethod": "client_secret_post",
            "authorizationUrl": f"{base}/oauth2/v2.0/authorize",
            "tokenUrl": f"{base}/oauth2/v2.0/token",
            "jwksUrl": f"{base}/discovery/v2.0/keys",
            "issuer": f"{base}/v2.0",
            "userInfoUrl": "https://graph.microsoft.com/oidc/userinfo",
            "logoutUrl": f"{base}/oauth2/v2.0/logout",
            "defaultScope": "openid profile email offline_access",   # Entra issues refresh tokens only with offline_access
        }
    iss = f"{UP}/realms/{UP_REALM}"          # what the browser (and the id_token `iss`) sees
    back = f"{UP_INTERNAL}/realms/{UP_REALM}"  # what kc-hpm calls server to server
    return {
        "clientId": "hpm-keycloak-broker", "clientSecret": BROKER_SECRET,
        "clientAuthMethod": "client_secret_post",
        "authorizationUrl": f"{iss}/protocol/openid-connect/auth",
        "tokenUrl": f"{back}/protocol/openid-connect/token",
        "jwksUrl": f"{back}/protocol/openid-connect/certs",
        "issuer": iss,
        "userInfoUrl": f"{back}/protocol/openid-connect/userinfo",
        "logoutUrl": f"{iss}/protocol/openid-connect/logout",
        "defaultScope": "openid profile email",
    }


def grant_admin_api_scope(admin, client_id: str) -> None:
    """Keycloak 26.8: the admin REST API only accepts tokens carrying the audience added by the kc-admin-api scope."""
    scope = next(c["id"] for c in admin.get("/client-scopes") if c["name"] == "kc-admin-api")
    admin.req("PUT", f"/clients/{admin.client_uuid(client_id)}/default-client-scopes/{scope}")


def bootstrap_scopes() -> None:
    """Audience + MCP scopes. (Not in the realm import: a realm file that lists clientScopes drops Keycloak's built-ins.)"""
    a = hpm_admin()
    existing = {c["name"]: c["id"] for c in a.get("/client-scopes")}
    mcp = a.client_uuid("mcp-client")

    def ensure(name, mappers=None, default=False):
        if name not in existing:
            a.req("POST", "/client-scopes", json={
                "name": name, "protocol": "openid-connect",
                "attributes": {"include.in.token.scope": "false" if mappers else "true", "display.on.consent.screen": "false"},
                "protocolMappers": mappers or []})
            existing[name] = next(c["id"] for c in a.get("/client-scopes") if c["name"] == name)
        kind = "default" if default else "optional"
        a.req("PUT", f"/clients/{mcp}/{kind}-client-scopes/{existing[name]}")

    ensure("hpm-mcp-audience", default=True, mappers=[{
        "name": "aud-hpm-mcp", "protocol": "openid-connect", "protocolMapper": "oidc-audience-mapper",
        "config": {"included.client.audience": "hpm-mcp", "id.token.claim": "false", "access.token.claim": "true",
                   "introspection.token.claim": "true"}}])
    for sc in ("mcp:tools.read", "mcp:tools.write", "mcp:tools.execute", "mcp:admin", "mcp:auditor"):
        ensure(sc)
    grant_admin_api_scope(a, "hpm-mcp")                         # resource server calls the admin API (revoke)
    if os.environ.get("UPSTREAM", "fake") == "fake":
        grant_admin_api_scope(upstream_admin(), "hpm-reconciler")  # stands in for Graph app permissions
    print("scopes ready")


def configure(kind: str | None = None, *, sync_mode="LEGACY", backchannel_logout_from_upstream=False,
              min_validity_token: int | None = None) -> None:
    kind = kind or os.environ.get("UPSTREAM", "fake")
    a = hpm_admin()
    try:
        a.req("DELETE", f"/identity-provider/instances/{ALIAS}")
    except RuntimeError:
        pass
    cfg = {
        "validateSignature": "true", "useJwksUrl": "true", "pkceEnabled": "true", "pkceMethod": "S256",
        "syncMode": sync_mode, "backchannelSupported": "false", "disableUserInfo": "false",
        "sendClientIdOnLogout": "true", "sendIdTokenOnLogout": "true",
        "allowedClockSkew": "30",
        **upstream_config(kind),
    }
    if min_validity_token is not None:   # seconds; > upstream access-token lifetime forces an upstream refresh on every /broker/entra/token
        cfg["minValidityToken"] = str(min_validity_token)
    a.req("POST", "/identity-provider/instances", json={
        "alias": ALIAS, "displayName": "Entra (stand-in)" if kind == "fake" else "Microsoft Entra ID",
        "providerId": "oidc", "enabled": True, "trustEmail": True, "storeToken": True,
        "addReadTokenRoleOnCreate": True, "firstBrokerLoginFlowAlias": "first broker login",
        "config": cfg})
    for claim_value, role in ROLE_MAP.items():
        a.req("POST", f"/identity-provider/instances/{ALIAS}/mappers", json={
            "name": f"role-{claim_value}", "identityProviderAlias": ALIAS,
            "identityProviderMapper": "oidc-role-idp-mapper",
            "config": {"claim": "roles", "claim.value": claim_value, "role": role, "syncMode": "INHERIT"}})
    if kind == "fake":
        # Entra cannot do this (no OIDC back-channel logout); the fake can, so it is a measured option.
        up = upstream_admin()
        cu = up.client_uuid("hpm-keycloak-broker")
        c = up.get(f"/clients/{cu}")
        attrs = c.setdefault("attributes", {})
        if backchannel_logout_from_upstream:
            attrs["backchannel.logout.url"] = f"{HPM_INTERNAL}/realms/{HPM_REALM}/protocol/openid-connect/logout/backchannel-logout"
            attrs["backchannel.logout.session.required"] = "true"
        else:
            attrs["backchannel.logout.url"] = ""   # an absent attribute is NOT removed by a client PUT; blank it
        up.req("PUT", f"/clients/{cu}", json=c)
    print(f"IdP '{ALIAS}' configured: upstream={kind} sync={sync_mode} upstream-backchannel={backchannel_logout_from_upstream} minValidityToken={min_validity_token}")


if __name__ == "__main__":
    wait_ready()
    bootstrap_scopes()
    configure()

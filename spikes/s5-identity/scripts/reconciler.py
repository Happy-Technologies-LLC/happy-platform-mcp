#!/usr/bin/env python3
"""Server-side offboarding enforcement: periodically re-checks the upstream IdP for every principal that holds
an hpm session, and revokes the hpm side when the upstream says the user is gone.

This is the behaviour the recommendation adds on top of stock Keycloak (which never re-checks the upstream
after the first login). It models what hpm-enterprise (or a Keycloak event-listener/SPI) would run.

Two strategies, both measured in P1-S5:

  graph         Poll the upstream directory as an app (Entra: Microsoft Graph, client credentials,
                User.Read.All): accountEnabled, 404 = deleted, appRoleAssignments, signInSessionsValidFromDateTime.
                Fake-entra equivalents: GET /users/{id} (enabled, 404), role mappings, user sessions.
                Needs no user token. Detects every offboarding event.
  broker-token  Call hpm's stored-token endpoint (GET /realms/hpm/broker/entra/token) with the user's own bearer
                token. With IdP config minValidityToken > upstream access-token lifetime, Keycloak performs an
                upstream refresh_token grant on every call; 502 = upstream refused. Also parses the `roles` claim of
                the refreshed upstream token. Detects disable/delete/session revoke through the upstream's own
                refresh semantics, so it also catches whatever the upstream decides (Conditional Access, risk).

On a violation it acts at hpm with the resource-server service account (`hpm-mcp`, realm-management
manage-users): logout all sessions of the user (refresh fails, introspection inactive), and revoke offline tokens (consents), disable the hpm
user (disabled/deleted) or drop the hpm role (role removed).
"""
from __future__ import annotations

import argparse
import random
import threading
import time

import requests

from configure_hpm import ALIAS, ROLE_MAP
from lib import (HPM, HPM_REALM, ISSUER, RECONCILER_SECRET, HPM_MCP_SECRET, RS_CLIENT, UP, UP_REALM,
                 Admin, jwt_claims, session, wait_ready)

REQUIRED_UPSTREAM_ROLE = "hpm.user"


def hpm_service_admin() -> Admin:
    return Admin(HPM, HPM_REALM, client_id=RS_CLIENT, client_secret=HPM_MCP_SECRET, token_realm=HPM_REALM)


def upstream_reader() -> Admin:
    return Admin(UP, UP_REALM, client_id="hpm-reconciler", client_secret=RECONCILER_SECRET, token_realm=UP_REALM)


class Reconciler(threading.Thread):
    def __init__(self, strategy: str, interval: float, latest_at: dict | None = None, *, phase: float | None = None, quiet=True):
        super().__init__(daemon=True)
        self.strategy, self.interval, self.latest_at = strategy, interval, latest_at if latest_at is not None else {}
        self.hp, self.up = hpm_service_admin(), upstream_reader()
        self.mcp_uuid = self.hp.client_uuid("mcp-client")
        self.stop_evt = threading.Event()
        self.actions: dict[str, tuple[str, float]] = {}    # hpm user id -> (verdict, monotonic time)
        self.scans = 0
        self.last_scan_s = 0.0
        self.phase = random.uniform(0, interval) if phase is None else phase
        self._fed: dict[str, str | None] = {}
        self.quiet = quiet
        self.http = session()

    def stop(self):
        self.stop_evt.set()

    def run(self):
        if self.stop_evt.wait(self.phase):
            return
        while not self.stop_evt.is_set():
            try:
                self.scan()
            except Exception as e:  # keep going; a real implementation would alert
                print(f"[reconciler] scan error: {e}")
            if self.stop_evt.wait(self.interval):
                return

    # ---- one pass
    def active_users(self) -> list[str]:
        sessions = self.hp.get(f"/clients/{self.mcp_uuid}/user-sessions?max=2000")
        return sorted({s["userId"] for s in sessions})

    def upstream_id(self, hpm_uid: str) -> str | None:
        if hpm_uid not in self._fed:
            ids = [f["userId"] for f in self.hp.get(f"/users/{hpm_uid}/federated-identity") if f["identityProvider"] == ALIAS]
            self._fed[hpm_uid] = ids[0] if ids else None
        return self._fed[hpm_uid]

    def scan(self):
        self.scans += 1
        t = time.monotonic()
        try:
            self._scan()
        finally:
            self.last_scan_s = time.monotonic() - t

    def _scan(self):
        for uid in self.active_users():
            if uid in self.actions:
                continue
            up_id = self.upstream_id(uid)
            if up_id is None:
                continue
            verdict = self.check_graph(up_id) if self.strategy == "graph" else self.check_broker(uid)
            if verdict:
                self.revoke(uid, verdict)

    # ---- strategies
    def check_graph(self, up_id: str) -> str | None:
        r = self.up.req("GET", f"/users/{up_id}", ok=(200, 404))
        if r.status_code == 404:
            return "upstream-deleted"
        if not r.json().get("enabled", True):
            return "upstream-disabled"
        if REQUIRED_UPSTREAM_ROLE not in self.up.user_realm_roles(up_id):
            return "upstream-role-removed"
        if not self.up.user_sessions(up_id):     # Entra: signInSessionsValidFromDateTime newer than the hpm session start
            return "upstream-sessions-revoked"
        return None

    def check_broker(self, hpm_uid: str) -> str | None:
        at = self.latest_at.get(hpm_uid)
        if not at:
            return None
        r = self.http.get(f"{ISSUER}/broker/{ALIAS}/token", headers={"Authorization": f"Bearer {at}"})
        if r.status_code != 200:
            return f"upstream-refresh-refused({r.status_code})"
        try:
            roles = jwt_claims(r.json()["access_token"]).get("roles", [])
        except Exception:
            return None
        return None if REQUIRED_UPSTREAM_ROLE in roles else "upstream-role-removed"

    # ---- action at hpm
    def revoke(self, hpm_uid: str, verdict: str):
        if verdict.startswith("upstream-role-removed"):
            for up_role, hpm_role in ROLE_MAP.items():
                if up_role == REQUIRED_UPSTREAM_ROLE:
                    try:
                        self.hp.remove_realm_role(hpm_uid, hpm_role)
                    except RuntimeError:
                        pass
        if verdict.startswith(("upstream-disabled", "upstream-deleted", "upstream-refresh-refused")):
            self.hp.set_enabled(hpm_uid, False)
        self.hp.logout_user(hpm_uid)
        # offline_access tokens live in offline sessions that the logout above does not touch: revoke them via consents
        for c in self.hp.get(f"/users/{hpm_uid}/consents"):
            self.hp.req("DELETE", f"/users/{hpm_uid}/consents/{c['clientId']}", ok=(200, 204, 404))
        self.actions[hpm_uid] = (verdict, time.monotonic())
        if not self.quiet:
            print(f"[reconciler] {hpm_uid}: {verdict} -> sessions revoked")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--strategy", choices=["graph"], default="graph", help="standalone mode supports graph only (broker-token needs the user's token)")
    ap.add_argument("--interval", type=float, default=30)
    a = ap.parse_args()
    wait_ready()
    r = Reconciler(a.strategy, a.interval, phase=0, quiet=False)
    print(f"reconciler: strategy={a.strategy} interval={a.interval}s (ctrl-c to stop)")
    r.start()
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        r.stop()

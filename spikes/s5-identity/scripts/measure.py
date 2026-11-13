#!/usr/bin/env python3
"""P1-S5 offboarding-window measurement.

For each hpm *profile* (a set of Keycloak/server settings) and each upstream *event* (disable, role removal,
session revocation, delete) it logs a fresh user in headlessly, triggers the event upstream, then polls five
channels once per tick until each one is cut off (or the window ends):

  a   refresh      refresh_token grant at hpm (the MCP client keeps refreshing, i.e. the worst-case active client)
  b   introspect   RFC 7662 introspection of an access token minted *just before* the event
  c1  new login    fresh browser (no cookies): hpm -> upstream login form with the user's password
  c2  hpm SSO      browser that only has the hpm SSO cookie (hpm session alive, upstream cookie gone)
  c3  upstream SSO browser that only has the upstream SSO cookie (hpm session gone, upstream session alive)

For event "role removed" a channel counts as cut when the role claim disappears (or the request fails).

Time is compressed: lifetimes are 1/SCALE of the production values (SCALE=10: a 5 min access token is 30 s).
Production-equivalent time = measured seconds x SCALE (+ the polling granularity, up to 4 s x SCALE).
"""
from __future__ import annotations

import argparse
import concurrent.futures as cf
import json
import os
import random
import sys
import threading
import time
from dataclasses import dataclass, field, replace

from configure_hpm import configure
from lib import (HPM_REALM, UP_REALM, authorize, exchange_code, hpm_admin, introspect, jwt_claims, login, refresh,
                 session, upstream_admin, wait_ready)
from reconciler import Reconciler

SCALE = 10
WINDOW = float(os.environ.get("WINDOW", 150))   # seconds per scenario (150 s x 10 = 25 min production-equivalent)
TICK = 0.5
EVERY = {"a": 2.0, "b": 2.0, "c1": 4.0, "c2": 4.0, "c3": 4.0}
PW = "Passw0rd!"
EVENTS = ["E1-disable", "E2-role-removed", "E3-sessions-revoked", "E4-delete"]


@dataclass
class Profile:
    name: str
    desc: str
    at: int = 30                  # access token lifespan, s   (prod 300)
    idle: int = 180               # SSO session idle, s        (prod 1800)
    max: int = 2880               # SSO session max, s         (prod 28800 = 8 h)
    sync: str = "IMPORT"          # IdP sync mode (Keycloak default for a new IdP is IMPORT)
    bcl: bool = False             # upstream -> hpm OIDC back-channel logout
    recon: str | None = None      # None | graph | broker-token
    recon_interval: float = 30    # s (prod 300)
    min_validity: int | None = None
    offline: bool = False         # MCP client requests offline_access (extra cohort)
    events: tuple = tuple(EVENTS)
    repeat: int = 1


PROFILES = {p.name: p for p in [
    Profile("P0-stock", "Keycloak defaults + AT 5 min, SSO idle 30 min / max 8 h, IdP sync mode IMPORT"),
    Profile("P1-force", "P0 + IdP and role mappers sync mode FORCE", sync="FORCE"),
    Profile("P2-session-cap", "P1 + SSO session max 15 min / idle 10 min", sync="FORCE", idle=60, max=90),
    Profile("P3-backchannel", "P1 + upstream OIDC back-channel logout to hpm", sync="FORCE", bcl=True),
    Profile("P4-reconciler-graph", "P1 + server re-checks upstream directory every 5 min (Graph-style)", sync="FORCE",
            recon="graph", recon_interval=30, repeat=2),
    Profile("P5-broker-token", "P1 + server calls /broker/entra/token (forced upstream refresh) every 5 min", sync="FORCE",
            recon="broker-token", recon_interval=30, min_validity=86400, repeat=2),
    Profile("P6-offline", "P2 but the MCP client asked for offline_access", sync="FORCE", idle=60, max=90, offline=True,
            events=("E1-disable", "E4-delete")),
    Profile("P7-recommended", "P1 + SSO max 4 h + graph reconciler every 5 min; no back-channel logout (Entra has none)",
            sync="FORCE", idle=180, max=1440, recon="graph", recon_interval=30, repeat=2),
    Profile("P8-offline-reconciled", "P7 but the MCP client asked for offline_access", sync="FORCE", idle=180, max=1440,
            recon="graph", recon_interval=30, offline=True, events=("E1-disable", "E2-role-removed", "E3-sessions-revoked", "E4-delete")),
]}


class Scenario:
    def __init__(self, prof: Profile, event: str, idx: int, offline: bool, latest_at: dict, recon: Reconciler | None,
                 only: tuple | None = None):
        self.p, self.event, self.offline = prof, event, offline
        if offline and only is None and event != "E0-control":
            only = ("a", "b", "c1")   # offline_access: Keycloak sends SSO-cookie-only authorize requests back to the IdP (seen at baseline)
        self.only = only      # restrict channels (role removal: a login mutates the hpm user, so channels are isolated)
        self.name = f"{prof.name}/{event}{'/' + '+'.join(only) if only else ''}{'/offline' if offline else ''}#{idx}"
        self.user = f"s5-{prof.name[:2].lower()}-{event[:2].lower()}-{'o' if offline else 'n'}{idx}-{random.randrange(10**6)}"
        self.up, self.hp = upstream_admin(), hpm_admin()
        self.latest_at, self.recon = latest_at, recon
        self.res: dict = {}

    # -- setup
    def setup(self) -> bool:
        self.up_id = self.up.create_user(self.user, PW, roles=["hpm.user"])
        scope = "openid mcp:tools.read" + (" offline_access" if self.offline else "")
        self.scope = scope
        r, self.jar = login(self.user, PW, scope=scope)
        if not r.ok:
            self.res["setup_error"] = r.why
            return False
        self.tok = r.tokens
        self.sub = jwt_claims(self.tok["access_token"])["sub"]
        # c2 / c3 browsers: split the cookie jar by realm path
        self.jar_hpm, self.jar_up = session(), session()
        for (path, name), v in self.jar.ck.items():
            (self.jar_up if f"/realms/{UP_REALM}/" in path else self.jar_hpm).ck[(path, name)] = v
        ok, t2 = refresh(self.tok["refresh_token"])       # AT minted right before the event
        if not ok:
            self.res["setup_error"] = f"first refresh {t2}"
            return False
        self.tok = t2
        self.at_f = t2["access_token"]
        self.at_f_exp = jwt_claims(self.at_f)["exp"]
        self.latest_at[self.sub] = self.at_f
        if not introspect(self.at_f).get("active"):
            self.res["setup_error"] = "AT not active at baseline"
            return False
        return True

    def trigger(self):
        if self.event == "E0-control":
            pass          # no offboarding: every channel must stay open, else the channel itself is broken
        elif self.event == "E1-disable":
            self.up.set_enabled(self.up_id, False)
        elif self.event == "E2-role-removed":
            self.up.remove_realm_role(self.up_id, "hpm.user")
        elif self.event == "E3-sessions-revoked":
            self.up.logout_user(self.up_id)
        elif self.event == "E4-delete":
            self.up.delete_user(self.up_id)
        self.t0 = time.monotonic()
        self.wall0 = time.time()

    # -- channels. Each returns None while access continues, else a short reason string.
    def _roles_gone(self, at) -> bool:
        return "mcp-user" not in jwt_claims(at).get("realm_access", {}).get("roles", [])

    def ch_a(self):
        ok, r = refresh(self.tok["refresh_token"])
        if not ok:
            return f"refresh {r.get('status')} {r.get('error_description') or r.get('error')}"
        self.tok = r
        self.latest_at[self.sub] = r["access_token"]
        if self.event == "E2-role-removed" and self._roles_gone(r["access_token"]):
            return "role absent from refreshed token"
        return None

    def ch_b(self):
        j = introspect(self.at_f)
        if not j.get("active"):
            early = time.time() < self.at_f_exp - 2
            return "inactive before exp (revoked)" if early else "inactive at exp (expiry only)"
        return None

    def _browser(self, jar, creds: bool):
        code, why, verifier, _ = authorize(jar, self.user if creds else None, PW if creds else None, scope=self.scope)
        if not code:
            return why
        tok = exchange_code(code, verifier)
        if tok.get("status") != 200:
            return f"token exchange {tok.get('error')}: {tok.get('error_description')}"
        if self.event == "E2-role-removed" and self._roles_gone(tok["access_token"]):
            return "role absent from new token"
        return None

    def ch_c1(self):
        if self.event == "E3-sessions-revoked":
            return "n/a"      # revoking sessions does not stop a fresh password login; not an offboarding control
        return self._browser(session(), True)

    def ch_c2(self):
        return self._browser(self.jar_hpm, False)

    def ch_c3(self):
        return self._browser(self.jar_up, False)

    def poll(self):
        chans = {"a": self.ch_a, "b": self.ch_b, "c1": self.ch_c1, "c2": self.ch_c2, "c3": self.ch_c3}
        if self.only:
            chans = {k: v for k, v in chans.items() if k in self.only}
        nxt = {k: 0.0 for k in chans}
        # stagger c-channels so they don't all fire on the same tick
        nxt["c2"], nxt["c3"] = 1.3, 2.6
        done: dict[str, dict] = {}
        while len(done) < len(chans):
            el = time.monotonic() - self.t0
            if el > WINDOW:
                break
            for k, fn in chans.items():
                if k in done or el < nxt[k]:
                    continue
                nxt[k] = el + EVERY[k]
                try:
                    why = fn()
                except Exception as e:
                    why = f"error {type(e).__name__}: {e}"
                if why:
                    done[k] = {"t": None if why == "n/a" else round(time.monotonic() - self.t0, 1), "why": why}
            time.sleep(TICK)
        for k in chans:
            self.res[k] = done.get(k, {"t": None, "why": f"not cut within {WINDOW:.0f} s"})
        self.res["at_f_exp_after_event_s"] = round(self.at_f_exp - self.wall0, 1)
        if self.recon is not None:
            act = self.recon.actions.get(self.sub_hpm_id)
            if act:
                self.res["reconciler"] = {"verdict": act[0], "t": round(act[1] - self.t0, 1),
                                          "last_scan_s": round(self.recon.last_scan_s, 2)}

    def run(self) -> dict:
        try:
            if not self.setup():
                return {"scenario": self.name, **self.res}
            self.sub_hpm_id = self.sub
            self.trigger()
            self.poll()
        except Exception as e:  # a harness failure must not look like "never cut"
            self.res["harness_error"] = f"{type(e).__name__}: {e}"
        return {"scenario": self.name, "profile": self.p.name, "event": self.event, "offline": self.offline, **self.res}


def apply_profile(p: Profile):
    hp = hpm_admin()
    hp.update_realm(accessTokenLifespan=p.at, ssoSessionIdleTimeout=p.idle, ssoSessionMaxLifespan=p.max,
                    revokeRefreshToken=True, refreshTokenMaxReuse=0,
                    offlineSessionIdleTimeout=max(p.idle, 60), offlineSessionMaxLifespanEnabled=False)
    configure("fake", sync_mode=p.sync, backchannel_logout_from_upstream=p.bcl, min_validity_token=p.min_validity)
    time.sleep(1)


def run_profile(p: Profile, out: list):
    print(f"\n=== {p.name}: {p.desc}", flush=True)
    apply_profile(p)
    latest_at: dict = {}
    recon = Reconciler(p.recon, p.recon_interval, latest_at) if p.recon else None
    if recon:
        recon.start()
    scenarios = []
    for event in p.events:
        splits = [("a", "b"), ("c1",), ("c2",), ("c3",)] if event == "E2-role-removed" else [None]
        for only in splits:
            for i in range(p.repeat):
                scenarios.append(Scenario(p, event, i, False, latest_at, recon, only))
        if p.offline:   # offline cohort: refresh + introspection + fresh login only
            for only in ([("a", "b"), ("c1",)] if event == "E2-role-removed" else [None]):
                scenarios.append(Scenario(p, event, 0, True, latest_at, recon, only))
    # negative control (no event): proves each channel stays open when nobody is offboarded
    scenarios.append(Scenario(p, "E0-control", 0, False, latest_at, recon, ("a", "b", "c1", "c2", "c3")))
    if p.offline:
        scenarios.append(Scenario(p, "E0-control", 0, True, latest_at, recon, ("a", "b", "c1", "c2", "c3")))
    if p.offline:   # in P6 only the offline cohort matters
        scenarios = [s for s in scenarios if s.offline]
    t = time.time()
    with cf.ThreadPoolExecutor(len(scenarios)) as ex:
        results = list(ex.map(lambda s: s.run(), scenarios))
    if recon:
        recon.stop()
    for r in results:
        print("  ", r["scenario"], {k: (v["t"], v["why"][:70]) for k, v in r.items() if k in EVERY} or r, flush=True)
    out.extend(results)
    print(f"   ({time.time() - t:.0f} s)", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profiles", nargs="*", default=list(PROFILES), help=f"any of {list(PROFILES)}")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    wait_ready()
    from configure_hpm import bootstrap_scopes
    bootstrap_scopes()
    out: list = []
    for name in a.profiles:
        run_profile(PROFILES[name], out)
    path = a.out or os.path.join(os.path.dirname(__file__), "..", "results", f"run-{time.strftime('%Y%m%d-%H%M%S')}.json")
    with open(path, "w") as f:
        json.dump({"scale": SCALE, "window_s": WINDOW, "profiles": {k: PROFILES[k].__dict__ for k in a.profiles}, "results": out}, f, indent=1)
    print("wrote", os.path.abspath(path))


if __name__ == "__main__":
    main()

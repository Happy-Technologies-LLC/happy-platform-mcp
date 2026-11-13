"""Shared helpers for the P1-S5 identity spike.

Everything the scripts need: Keycloak admin REST client, the headless browser login
(auth code + PKCE, brokered to the upstream IdP by scripted form posts), token
operations at the hpm realm, and the upstream (fake-entra) "tenant admin" operations
that stand in for Entra / Graph calls.

All endpoints and secrets are env-configurable so a later run can point the broker at
a real Entra tenant (see configure_hpm.py).
"""
from __future__ import annotations

import base64
import hashlib
import html.parser
import os
import secrets
import time
import urllib.parse
from dataclasses import dataclass, field

import requests

HPM = os.environ.get("HPM_BASE", "http://localhost:8080")            # browser-facing issuer base
HPM_REALM = os.environ.get("HPM_REALM", "hpm")
UP = os.environ.get("FAKE_ENTRA_BASE", "http://localhost:8081")       # fake-entra, as seen from the host
UP_REALM = os.environ.get("FAKE_ENTRA_REALM", "fake-entra")
# How kc-hpm (inside the compose network) reaches the upstream, server to server.
UP_INTERNAL = os.environ.get("FAKE_ENTRA_INTERNAL", "http://fake-entra:8080")
HPM_INTERNAL = os.environ.get("HPM_INTERNAL", "http://kc-hpm:8080")

HPM_MCP_SECRET = os.environ.get("HPM_MCP_SECRET", "hpm-mcp-dev-secret")
BROKER_SECRET = os.environ.get("BROKER_SECRET", "broker-dev-secret")
RECONCILER_SECRET = os.environ.get("RECONCILER_SECRET", "reconciler-dev-secret")
ADMIN_USER = os.environ.get("KC_ADMIN", "admin")
ADMIN_PASS = os.environ.get("KC_ADMIN_PASSWORD", "admin")

REDIRECT_URI = "http://127.0.0.1:8765/callback"                       # never served; we read the Location header
MCP_CLIENT = "mcp-client"
RS_CLIENT = "hpm-mcp"
ISSUER = f"{HPM}/realms/{HPM_REALM}"
TOKEN_URL = f"{ISSUER}/protocol/openid-connect/token"
INTROSPECT_URL = f"{TOKEN_URL}/introspect"
AUTH_URL = f"{ISSUER}/protocol/openid-connect/auth"
HTTP_TIMEOUT = 20


class _S(requests.Session):
    """requests session with a tiny browser-like cookie store.

    Keycloak marks cookies Secure even on http://localhost (browsers accept that on localhost; Python's
    cookiejar and requests do not), and cookies are shared across ports for one host, so scope by path only.
    """

    def __init__(self):
        super().__init__()
        self.ck: dict[tuple[str, str], str] = {}   # (path, name) -> value

    def request(self, *a, **kw):  # default timeout everywhere
        kw.setdefault("timeout", HTTP_TIMEOUT)
        return super().request(*a, **kw)

    def prepare_request(self, request):
        p = super().prepare_request(request)
        path = urllib.parse.urlparse(p.url).path
        pairs = [f"{n}={v}" for (cp, n), v in self.ck.items() if path.startswith(cp)]
        if pairs:
            p.headers["Cookie"] = "; ".join(pairs)
        return p

    def send(self, request, **kw):
        r = super().send(request, **kw)
        for h in r.raw.headers.getlist("Set-Cookie"):
            parts = [x.strip() for x in h.split(";")]
            name, _, val = parts[0].partition("=")
            attrs = {k.lower(): v for k, _, v in (x.partition("=") for x in parts[1:])}
            path = attrs.get("path", "/")
            if attrs.get("max-age") == "0" or val in ("", '""'):
                self.ck.pop((path, name), None)
            else:
                self.ck[(path, name)] = val
        return r


def session() -> requests.Session:
    s = _S()
    s.headers["User-Agent"] = "hpm-s5-spike"
    return s


# --------------------------------------------------------------------------- admin REST
class Admin:
    """Keycloak admin REST for one server. `realm` is the realm being administered."""

    def __init__(self, base: str, realm: str, *, user=None, password=None,
                 client_id="admin-cli", client_secret=None, token_realm="master"):
        self.base, self.realm = base, realm
        self.s = session()
        self._tok = None
        self._exp = 0.0
        self._cred = dict(user=user, password=password, client_id=client_id,
                          client_secret=client_secret, token_realm=token_realm)

    def _token(self):
        if self._tok and time.time() < self._exp - 5:
            return self._tok
        c = self._cred
        data = {"client_id": c["client_id"]}
        if c["user"]:
            data.update(grant_type="password", username=c["user"], password=c["password"])
        else:
            data.update(grant_type="client_credentials", client_secret=c["client_secret"])
        r = self.s.post(f"{self.base}/realms/{c['token_realm']}/protocol/openid-connect/token", data=data)
        r.raise_for_status()
        j = r.json()
        self._tok, self._exp = j["access_token"], time.time() + j.get("expires_in", 60)
        return self._tok

    def req(self, method, path, ok=(200, 201, 204), **kw):
        url = f"{self.base}/admin/realms/{self.realm}{path}"
        r = self.s.request(method, url, headers={"Authorization": f"Bearer {self._token()}"}, **kw)
        if r.status_code not in ok:
            raise RuntimeError(f"{method} {url} -> {r.status_code} {r.text[:300]}")
        return r

    def get(self, path, **kw):
        return self.req("GET", path, **kw).json()

    # users
    def create_user(self, username, password, *, email=None, first="Test", last="User", roles=(), attributes=None):
        email = email or f"{username}@example.test"
        body = {"username": username, "email": email, "emailVerified": True, "firstName": first,
                "lastName": last, "enabled": True,
                "credentials": [{"type": "password", "value": password, "temporary": False}]}
        if attributes:
            body["attributes"] = attributes
        r = self.req("POST", "/users", json=body)
        uid = r.headers["Location"].rsplit("/", 1)[1]
        for role in roles:
            self.add_realm_role(uid, role)
        return uid

    def find_user(self, username):
        r = self.get(f"/users?username={urllib.parse.quote(username)}&exact=true")
        return r[0] if r else None

    def set_enabled(self, uid, enabled):
        u = self.get(f"/users/{uid}")
        u["enabled"] = enabled
        self.req("PUT", f"/users/{uid}", json=u)

    def delete_user(self, uid):
        self.req("DELETE", f"/users/{uid}")

    def logout_user(self, uid):
        """Admin 'sign out all sessions' (removes sessions; sends backchannel logout to clients that have a URL)."""
        self.req("POST", f"/users/{uid}/logout")

    def user_sessions(self, uid):
        return self.get(f"/users/{uid}/sessions")

    def realm_role(self, name):
        return self.get(f"/roles/{urllib.parse.quote(name)}")

    def add_realm_role(self, uid, name):
        self.req("POST", f"/users/{uid}/role-mappings/realm", json=[self.realm_role(name)])

    def remove_realm_role(self, uid, name):
        self.req("DELETE", f"/users/{uid}/role-mappings/realm", json=[self.realm_role(name)])

    def user_realm_roles(self, uid):
        return [r["name"] for r in self.get(f"/users/{uid}/role-mappings/realm")]

    def update_realm(self, **fields):
        r = self.get("")
        r.update(fields)
        self.req("PUT", "", json=r)

    def client_uuid(self, client_id):
        return self.get(f"/clients?clientId={client_id}")[0]["id"]


def hpm_admin() -> Admin:
    return Admin(HPM, HPM_REALM, user=ADMIN_USER, password=ADMIN_PASS)


def upstream_admin() -> Admin:
    """The 'tenant admin' on the fake Entra (Entra: Graph with User.ReadWrite.All + admin UI)."""
    return Admin(UP, UP_REALM, user=ADMIN_USER, password=ADMIN_PASS)


# --------------------------------------------------------------------------- headless login
class _Form(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.forms, self._cur = [], None

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "form":
            self._cur = {"action": a.get("action", ""), "inputs": {}}
            self.forms.append(self._cur)
        elif tag == "input" and self._cur is not None and a.get("name"):
            self._cur["inputs"][a["name"]] = a.get("value", "")

    def handle_endtag(self, tag):
        if tag == "form":
            self._cur = None


def _form(body: str):
    p = _Form()
    p.feed(body)
    return p.forms[0] if p.forms else None


def pkce():
    v = base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode()
    c = base64.urlsafe_b64encode(hashlib.sha256(v.encode()).digest()).rstrip(b"=").decode()
    return v, c


@dataclass
class LoginResult:
    ok: bool
    why: str = ""
    tokens: dict = field(default_factory=dict)
    seconds: float = 0.0


def authorize(jar: requests.Session, username: str | None, password: str | None, *,
              idp_hint: str | None = "entra", scope="openid mcp:tools.read",
              prompt: str | None = None, max_hops=14) -> tuple[str | None, str, str, str]:
    """Run /auth in `jar` (cookie jar = browser). With credentials, post the upstream login form.

    Returns (code | None, why, verifier, state). `why` explains a failure (which page we got stuck on).
    """
    verifier, challenge = pkce()
    state = secrets.token_urlsafe(8)
    q = {"client_id": MCP_CLIENT, "response_type": "code", "redirect_uri": REDIRECT_URI, "scope": scope,
         "code_challenge": challenge, "code_challenge_method": "S256", "state": state}
    if idp_hint:
        q["kc_idp_hint"] = idp_hint
    if prompt:
        q["prompt"] = prompt
    url = f"{AUTH_URL}?{urllib.parse.urlencode(q)}"
    method, data = "GET", None
    posted_creds = False
    for _ in range(max_hops):
        r = jar.request(method, url, data=data, allow_redirects=False)
        if r.status_code in (301, 302, 303, 307, 308):
            loc = urllib.parse.urljoin(url, r.headers["Location"])
            if loc.startswith(REDIRECT_URI):
                qs = urllib.parse.parse_qs(urllib.parse.urlparse(loc).query)
                if "code" in qs:
                    return qs["code"][0], "ok", verifier, state
                return None, f"redirect_error:{qs.get('error', ['?'])[0]}:{qs.get('error_description', [''])[0]}", verifier, state
            url, method, data = loc, "GET", None
            continue
        if r.status_code == 200:
            f = _form(r.text)
            if f is None:
                return None, f"page_without_form@{urllib.parse.urlparse(url).netloc}", verifier, state
            fields = dict(f["inputs"])
            if "username" in fields and "password" in fields:
                if username is None or posted_creds:
                    msg = _err_text(r.text) or "login page (credentials rejected)"
                    return None, f"login_rejected@{urllib.parse.urlparse(url).netloc}:{msg}", verifier, state
                fields["username"], fields["password"] = username, password
                posted_creds = True
            elif username is None:
                return None, "needs_interaction", verifier, state
            else:
                # e.g. 'review profile' / 'link account' intermediate page: we cannot proceed headlessly
                return None, f"unexpected_form:{sorted(fields)}:{_err_text(r.text)}", verifier, state
            url, method, data = urllib.parse.urljoin(url, f["action"].replace("&amp;", "&")), "POST", fields
            continue
        return None, f"http_{r.status_code}@{urllib.parse.urlparse(url).netloc}:{_err_text(r.text)}", verifier, state
    return None, "too_many_redirects", verifier, state


def _err_text(body: str) -> str:
    import re
    m = re.search(r'kc-feedback-text[^>]*>(.*?)</span>', body, re.S) or re.search(r'<title>(.*?)</title>', body, re.S)
    return re.sub(r"<[^>]+>|\s+", " ", m.group(1)).strip() if m else ""


def exchange_code(code: str, verifier: str) -> dict:
    r = session().post(TOKEN_URL, data={"grant_type": "authorization_code", "client_id": MCP_CLIENT, "code": code,
                                         "redirect_uri": REDIRECT_URI, "code_verifier": verifier})
    return {"status": r.status_code, **r.json()} if r.headers.get("content-type", "").startswith("application/json") else {"status": r.status_code}


def login(username, password, jar=None, **kw) -> tuple[LoginResult, requests.Session]:
    """Full brokered login: hpm /auth -> (kc_idp_hint) fake-entra form -> hpm -> code -> tokens."""
    jar = jar or session()
    t = time.monotonic()
    code, why, verifier, _ = authorize(jar, username, password, **kw)
    if not code:
        return LoginResult(False, why, seconds=time.monotonic() - t), jar
    tok = exchange_code(code, verifier)
    if tok.get("status") != 200:
        return LoginResult(False, f"token_exchange:{tok.get('error')}:{tok.get('error_description')}", seconds=time.monotonic() - t), jar
    return LoginResult(True, "ok", tok, time.monotonic() - t), jar


def refresh(rt: str) -> tuple[bool, dict]:
    r = session().post(TOKEN_URL, data={"grant_type": "refresh_token", "client_id": MCP_CLIENT, "refresh_token": rt})
    j = r.json() if r.content else {}
    return r.status_code == 200, {"status": r.status_code, **j}


def introspect(at: str) -> dict:
    """RFC 7662 introspection as the resource server (client hpm-mcp, client_secret_basic)."""
    r = session().post(INTROSPECT_URL, data={"token": at}, auth=(RS_CLIENT, HPM_MCP_SECRET))
    r.raise_for_status()
    return r.json()


def jwt_claims(tok: str) -> dict:
    import json
    p = tok.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))


def wait_ready(timeout=120):
    end = time.time() + timeout
    s = session()
    for url in (f"{HPM}/realms/{HPM_REALM}/.well-known/openid-configuration",
                f"{UP}/realms/{UP_REALM}/.well-known/openid-configuration"):
        while True:
            try:
                if s.get(url).status_code == 200:
                    break
            except requests.RequestException:
                pass
            if time.time() > end:
                raise TimeoutError(url)
            time.sleep(2)

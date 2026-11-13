#!/usr/bin/env python3
"""Refresh-token rotation behaviour at hpm (revokeRefreshToken=true, refreshTokenMaxReuse=0).

Shows: (1) a rotated refresh token is single-use; (2) replaying an old one fails AND kills the session, so a stolen
or duplicated token cannot silently live in parallel with the legitimate chain. Not an offboarding control by
itself: the legitimate chain keeps rotating until the session ends.
"""
import time
from lib import hpm_admin, login, refresh, upstream_admin, wait_ready

wait_ready()
up = upstream_admin()
name = f"s5-rot-{int(time.time())}"
up.create_user(name, "Passw0rd!", roles=["hpm.user"])
r, _ = login(name, "Passw0rd!")
rt0 = r.tokens["refresh_token"]
ok1, t1 = refresh(rt0)
print("refresh with RT0:", ok1)
ok_reuse, e = refresh(rt0)
print("replay RT0 :", ok_reuse, e.get("error"), "-", e.get("error_description"))
ok2, e2 = refresh(t1["refresh_token"])
print("RT1 after replay of RT0:", ok2, e2.get("error"), "-", e2.get("error_description"))

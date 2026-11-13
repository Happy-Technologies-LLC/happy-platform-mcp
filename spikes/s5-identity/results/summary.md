
### P0-stock

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 2880 s, sync IMPORT, upstream back-channel logout False, recon None every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.7 s ≈ 0.5 min |
| E2-role-removed | **not cut in 150 s** (>25 min) | 32.4 s (= AT expiry) | **not cut in 150 s** (>25 min) | **not cut in 150 s** (>25 min) | **not cut in 150 s** (>25 min) |
| E3-sessions-revoked | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | n/a (login allowed) | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E4-delete | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.7 s ≈ 0.5 min |

### P1-force

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 2880 s, sync FORCE, upstream back-channel logout False, recon None every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | **not cut in 150 s** (>25 min) | 32.7 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.7 s ≈ 0.5 min |
| E2-role-removed | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | **not cut in 150 s** (>25 min) | 32.7 s (= AT expiry) | n/a (login allowed) | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E4-delete | **not cut in 150 s** (>25 min) | 32.7 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.7 s ≈ 0.5 min |

### P2-session-cap

Settings (test values; x10 = production): AT 30 s, SSO idle 60 s, SSO max 90 s, sync FORCE, upstream back-channel logout False, recon None every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | 89.6 s ≈ 15 min | 30.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | 91.2 s ≈ 15 min | 2.7 s ≈ 0.5 min |
| E2-role-removed | 89.2 s ≈ 15 min | 30.5 s (= AT expiry) | 0.1 s ≈ 0.0 min | 90.6 s ≈ 15 min | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | 89.6 s ≈ 15 min | 30.6 s (= AT expiry) | n/a (login allowed) | 91.2 s ≈ 15 min | 3.1 s ≈ 0.5 min |
| E4-delete | 89.7 s ≈ 15 min | 30.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | 91.2 s ≈ 15 min | 2.7 s ≈ 0.5 min |

### P3-backchannel

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 2880 s, sync FORCE, upstream back-channel logout True, recon None every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E2-role-removed | **not cut in 150 s** (>25 min) | 32.4 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | n/a (login allowed) | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E4-delete | **not cut in 150 s** (>25 min) | 32.7 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.6 s ≈ 0.4 min |

### P4-reconciler-graph

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 2880 s, sync FORCE, upstream back-channel logout False, recon graph every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | 12.3-14.5 s ≈ 2.4 min | 12.4-14.5 s ≈ 2.4 min | 0.1-0.2 s ≈ 0.0 min | 13.9-14 s ≈ 2.3 min | 2.7-2.8 s ≈ 0.5 min |
| E2-role-removed | 14.2 s ≈ 2.4 min | 14.2 s ≈ 2.4 min | 0.2-0.3 s ≈ 0.1 min | 13.7 s ≈ 2.3 min | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | 14.3 s ≈ 2.4 min | 14.3 s ≈ 2.4 min | n/a (login allowed) | 13.8 s ≈ 2.3 min | 3.1 s ≈ 0.5 min |
| E4-delete | 14.5 s ≈ 2.4 min | 14.5 s ≈ 2.4 min | 0.2 s ≈ 0.0 min | 14 s ≈ 2.3 min | 2.8 s ≈ 0.5 min |

### P5-broker-token

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 2880 s, sync FORCE, upstream back-channel logout False, recon broker-token every 30 s, minValidityToken 86400, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | 10.4-12.3 s ≈ 2.0 min | 10.4-12.3 s ≈ 2.0 min | 0.1-0.2 s ≈ 0.0 min | 13.9-14 s ≈ 2.3 min | 2.6-2.8 s ≈ 0.5 min |
| E2-role-removed | 10.2-12.3 s ≈ 2.0 min | 10.2-12.3 s ≈ 2.0 min | 0.1-0.2 s ≈ 0.0 min | 13.6-13.7 s ≈ 2.3 min | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | 10.2-12.3 s ≈ 2.0 min | 10.2-12.3 s ≈ 2.0 min | n/a (login allowed) | 13.8-13.9 s ≈ 2.3 min | 2.7-3.1 s ≈ 0.5 min |
| E4-delete | 10.4-12.3 s ≈ 2.0 min | 10.4-12.3 s ≈ 2.0 min | 0.1-0.2 s ≈ 0.0 min | 13.9 s ≈ 2.3 min | 2.7 s ≈ 0.5 min |

### P6-offline

Settings (test values; x10 = production): AT 30 s, SSO idle 60 s, SSO max 90 s, sync FORCE, upstream back-channel logout False, recon None every 30 s, minValidityToken None, offline_access True

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable (offline_access) | **not cut in 150 s** (>25 min) | 32.5 s (= AT expiry) | 0.1 s ≈ 0.0 min | - | - |
| E4-delete (offline_access) | **not cut in 150 s** (>25 min) | 32.5 s (= AT expiry) | 0.1 s ≈ 0.0 min | - | - |

### P7-recommended

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 1440 s, sync FORCE, upstream back-channel logout False, recon graph every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable | 14.4 s ≈ 2.4 min | 14.4 s ≈ 2.4 min | 0.1 s ≈ 0.0 min | 13.8-13.9 s ≈ 2.3 min | 2.6-2.7 s ≈ 0.5 min |
| E2-role-removed | 14.3 s ≈ 2.4 min | 14.3 s ≈ 2.4 min | 0.2-0.3 s ≈ 0.1 min | 13.7 s ≈ 2.3 min | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | 14.3-14.4 s ≈ 2.4 min | 14.3-14.4 s ≈ 2.4 min | n/a (login allowed) | 13.8-13.9 s ≈ 2.3 min | 3.1 s ≈ 0.5 min |
| E4-delete | 14.4-14.5 s ≈ 2.4 min | 14.4-14.5 s ≈ 2.4 min | 0.1 s ≈ 0.0 min | 13.9-14 s ≈ 2.3 min | 2.7 s ≈ 0.5 min |

### P8-offline-reconciled

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 1440 s, sync FORCE, upstream back-channel logout False, recon graph every 30 s, minValidityToken None, offline_access True

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E1-disable (offline_access) | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | - | - |
| E2-role-removed (offline_access) | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | - | - |
| E3-sessions-revoked (offline_access) | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | n/a (login allowed) | - | - |
| E4-delete (offline_access) | **not cut in 150 s** (>25 min) | 32.6 s (= AT expiry) | 0.1 s ≈ 0.0 min | - | - |

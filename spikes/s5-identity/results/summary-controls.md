
### P3-backchannel

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 2880 s, sync FORCE, upstream back-channel logout True, recon None every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E0-control | **not cut in 150 s** (>25 min) | 31.8 s (= AT expiry) | **not cut in 150 s** (>25 min) | **not cut in 150 s** (>25 min) | **not cut in 150 s** (>25 min) |
| E1-disable | **not cut in 150 s** (>25 min) | 30.9 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.7 s ≈ 0.5 min |
| E2-role-removed | **not cut in 150 s** (>25 min) | 32.7 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | 0 s ≈ 0.0 min | 0 s ≈ 0.0 min | n/a (login allowed) | 1.5 s ≈ 0.2 min | 3.1 s ≈ 0.5 min |
| E4-delete | **not cut in 150 s** (>25 min) | 30.9 s (= AT expiry) | 0.1 s ≈ 0.0 min | **not cut in 150 s** (>25 min) | 2.7 s ≈ 0.5 min |

### P7-recommended

Settings (test values; x10 = production): AT 30 s, SSO idle 180 s, SSO max 1440 s, sync FORCE, upstream back-channel logout False, recon graph every 30 s, minValidityToken None, offline_access False

| event | refresh at hpm | introspect existing AT | new login (fresh) | re-auth, hpm SSO only | re-auth, upstream SSO only |
|---|---|---|---|---|---|
| E0-control | **not cut in 150 s** (>25 min) | 31.6 s (= AT expiry) | **not cut in 150 s** (>25 min) | **not cut in 150 s** (>25 min) | **not cut in 150 s** (>25 min) |
| E1-disable | 4.2 s ≈ 0.7 min | 4.2 s ≈ 0.7 min | 0.1 s ≈ 0.0 min | 5.8 s ≈ 1.0 min | 2.7 s ≈ 0.5 min |
| E2-role-removed | 4.1 s ≈ 0.7 min | 4.1 s ≈ 0.7 min | 0.1 s ≈ 0.0 min | 5.6 s ≈ 0.9 min | 3.1 s ≈ 0.5 min |
| E3-sessions-revoked | 4.1 s ≈ 0.7 min | 4.1 s ≈ 0.7 min | n/a (login allowed) | 5.6 s ≈ 0.9 min | 3.1 s ≈ 0.5 min |
| E4-delete | 4.2 s ≈ 0.7 min | 4.2 s ≈ 0.7 min | 0.1 s ≈ 0.0 min | 5.7 s ≈ 0.9 min | 2.7 s ≈ 0.5 min |

#!/usr/bin/env bash
# ywl/zzp: bash ~/codex-proxy-rs/deploy/33_entry_diagnostics.sh tests|health|release
# Existing isolated builder only, locked release -j4. Never changes production.
set -euo pipefail
r=/home/zzp/codex-proxy-rs/.runtime/30_upstream_acceptance
b=cpr-upstream30-build
exec > >(tee -a "$r/entry-diagnostics-build.log") 2>&1
date -Is
case "${1:?tests|health|release}" in
tests)
 buildah copy "$b" "$r/source/backend" /app/backend >/dev/null
 buildah run --isolation chroot "$b" sh -ec '
 export PATH=/usr/local/cargo/bin:$PATH; cd /app/backend
 cargo test --release --locked -j4 -p gateway-core --test main blocked_model_provider_should_not_be_misreported_as_model_not_found -- --nocapture
 cargo test --release --locked -j4 -p gateway-core --test main known_catalog_should_reject_a_model_that_the_provider_did_not_publish -- --nocapture
 ' </dev/null
 ;;
health)
 buildah copy "$b" "$r/source/backend" /app/backend >/dev/null
 buildah run --isolation chroot "$b" sh -ec '
 export PATH=/usr/local/cargo/bin:$PATH; cd /app/backend
 cargo test --release --locked -j4 -p gateway-host --test main daemon_recovery_clears_stale_health_without_erasing_crash_backoff -- --nocapture
 cargo test --release --locked -j4 -p gateway-host --test main supervisor_uses_exponential_backoff_and_recovers_health -- --nocapture
 ' </dev/null
 ;;
release) bash /home/zzp/codex-proxy-rs/deploy/30_accept.sh backend ;;
*) echo 'expected tests|health|release' >&2; exit 2;;
esac
date -Is

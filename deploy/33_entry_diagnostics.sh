#!/usr/bin/env bash
# ywl/zzp: bash ~/codex-proxy-rs/.runtime/30_upstream_acceptance/source/deploy/33_entry_diagnostics.sh tests|health|catalog|release|image
# Existing isolated builder only, locked release -j4. Never changes production.
set -euo pipefail
r=/home/zzp/codex-proxy-rs/.runtime/30_upstream_acceptance
b=cpr-upstream30-build
exec > >(tee -a "$r/entry-diagnostics-build.log") 2>&1
date -Is
case "${1:?tests|health|catalog|release|image}" in
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
catalog)
 buildah copy "$b" "$r/source/backend" /app/backend >/dev/null
 buildah run --isolation chroot "$b" sh -ec '
 export PATH=/usr/local/cargo/bin:$PATH; cd /app/backend
 cargo test --release --locked -j4 -p provider-openai --test main plan_catalog_cache_is_shared_and_manual_refresh_replaces_it -- --nocapture
 ' </dev/null
 ;;
release) bash /home/zzp/codex-proxy-rs/deploy/30_accept.sh backend ;;
image)
 base=$(podman inspect cpr-iso-prodata_codex-proxy-rs_1 --format '{{.Image}}')
 c=$(buildah from "$base")
 buildah copy --chown 10001:10001 "$c" "$r/codex-proxy-rs" /app/bin/codex-proxy-rs >/dev/null
 sha=$(cut -c1-8 "$r/binary-source.txt")
 buildah config --label "io.molaison.acceptance=$sha" "$c"
 buildah commit "$c" "localhost/cpr-ywl:3.19.0-acceptance-$sha"
 printf 'localhost/cpr-ywl:3.19.0-acceptance-%s\n' "$sha" > "$r/image.txt"
 buildah rm "$c" >/dev/null
 ;;
*) echo 'expected tests|health|catalog|release|image' >&2; exit 2;;
esac
date -Is

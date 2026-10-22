# shellcheck shell=bash
#
# fetch_url URL OUT: download URL to the file OUT.
#
# Try a short, bounded direct request first. If that fails and a `with-proxy`
# wrapper command is on PATH (some networks reach the internet only through a
# forward proxy), retry through it. curl also honors any http_proxy,
# https_proxy, or ALL_PROXY setting. Returns nonzero with a message on stderr
# when the URL cannot be reached.

fetch_url() {
  local url="$1" out="$2"

  if curl --fail --location --silent --show-error --head \
       --connect-timeout "${QEMU_FETCH_CONNECT_TIMEOUT:-10}" \
       --max-time "${QEMU_FETCH_PROBE_TIMEOUT:-20}" \
       "$url" -o /dev/null 2>/dev/null; then
    curl --fail --location --silent --show-error "$url" --output "$out"
    return $?
  fi

  if command -v with-proxy >/dev/null 2>&1; then
    echo '  direct connection failed; retrying through with-proxy...' >&2
    with-proxy curl --fail --location --silent --show-error \
      "$url" --output "$out"
    return $?
  fi

  printf 'error: cannot reach %s: the direct connection failed and no with-proxy helper is on PATH. Set http(s)_proxy for your network, or provide the kernel locally with KERNEL_IMAGE=/path/to/bzImage.\n' \
    "$url" >&2
  return 1
}

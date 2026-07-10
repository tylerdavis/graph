# CI-oriented runtime image: graph plus the tools its github pack shells out
# to (git, jq, gh). Used as the `container:` for GitHub Actions jobs — see
# .github/workflows/graph-checks.yaml — and works standalone:
#   docker run --rm ghcr.io/tylerdavis/graph:latest --help
#
# The graph binary is downloaded from the GitHub release by the publishing
# job in .github/workflows/release.yaml and COPY'd in, keeping this file
# auth-free. Debian (glibc) base on purpose: GitHub Actions injects its own
# Node runtime into job containers, which breaks on musl.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git jq gh \
    && rm -rf /var/lib/apt/lists/* \
    # Actions job containers mount the workspace owned by a foreign uid;
    # without this every git invocation dies on the dubious-ownership check.
    && git config --system --add safe.directory '*'

COPY graph /usr/local/bin/graph

# Standalone UX: `docker run … ask "hi"` just works. Actions job containers
# override the entrypoint (tail -f /dev/null), so CI is unaffected.
ENTRYPOINT ["graph"]

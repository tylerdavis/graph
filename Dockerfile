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

# Links the ghcr package to this repository (automatic for GITHUB_TOKEN
# pushes; the label makes it hold for any push path).
LABEL org.opencontainers.image.source="https://github.com/tylerdavis/graph" \
      org.opencontainers.image.description="graph CLI with git, jq, and gh — ready for CI plan runs" \
      org.opencontainers.image.licenses="MIT"

# gh comes from the official releases, not Debian's archive — trixie ships
# 2.46, which predates JSON fields (baseRefOid) and flags (--create-if-none)
# the github tool pack depends on.
ARG GH_VERSION=2.96.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git jq \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz" \
       | tar xz --strip-components=2 -C /usr/local/bin "gh_${GH_VERSION}_linux_amd64/bin/gh" \
    # Actions job containers mount the workspace owned by a foreign uid;
    # without this every git invocation dies on the dubious-ownership check.
    && git config --system --add safe.directory '*'

COPY graph /usr/local/bin/graph

# Standalone UX: `docker run … ask "hi"` just works. Actions job containers
# override the entrypoint (tail -f /dev/null), so CI is unaffected.
ENTRYPOINT ["graph"]

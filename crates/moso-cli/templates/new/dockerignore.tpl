# What must never reach the build context.
#
# `target/` is the important line: it is gigabytes, it is rebuilt inside the
# image anyway, and sending it to the daemon makes `docker build` appear to hang
# before it has run a single instruction.
target/

# Secrets. `.env.example` is committed and safe; `.env` is neither, and a
# `COPY . .` would bake it into a layer that survives being deleted later.
.env
.env.*
!.env.example

# Version control and editor state.
.git/
.gitignore
.jj/
*.swp
*~

# Container and CI definitions: they describe the build, they are not inputs to
# it, and touching one should not invalidate every cached layer.
Dockerfile
.dockerignore
compose*.yaml
compose*.yml
.github/

# Documentation.
README.md
docs/

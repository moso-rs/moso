# Real configuration for this machine. Ignored by git; `.env.example` is the
# committed shape and `moso config` prints which source won for every key.
#
# The signing key below came from this machine's operating system CSPRNG when
# `moso new --auth` created the project, and it is this checkout's alone. It
# signs the session cookie, so replacing it logs everybody out — which is what
# you want if it ever leaks. Your deployment sets its own, from its own secret
# store, and never from a file in the repository.
#
#   moso config --generate-secret        # 32 fresh bytes, base64
@@ENV_PREFIX@@__SESSION_SECRET=base64:@@SESSION_SECRET@@

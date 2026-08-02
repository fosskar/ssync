# nixbot effects. The GitToken comes from nixbot at runtime (a
# github app installation token on github repos).
{ pkgs, nixbot }:
let
  inherit ((pkgs.lib.importTOML ../Cargo.toml).workspace.package) version;
  inherit (nixbot.lib.effects { inherit pkgs; }) mkEffect;
in
{ primaryRepo, ... }:
{
  # Auto-release on version bump: push effects run only after the whole build
  # (nix flake check) succeeded on that commit, so a broken bump can never
  # become a release. No-op while the workspace version is already tagged.
  onPush.default.outputs.effects = pkgs.lib.optionalAttrs (primaryRepo.branch or null == "main") {
    release = mkEffect {
      name = "effect-release";
      inputs = [ pkgs.gh ];
      secretsMap.git.type = "GitToken";
      effectScript = ''
        set -euo pipefail
        GH_TOKEN=$(jq -re '.git.data.token' "$HERCULES_CI_SECRETS_JSON")
        export GH_TOKEN

        if gh api "repos/fosskar/ssync/git/ref/tags/v${version}" > /dev/null 2>&1; then
          echo "v${version} already released"
        else
          gh release create "v${version}" --repo fosskar/ssync \
            --generate-notes --target "${primaryRepo.rev}"
        fi
      '';
    };
  };

  onSchedule.update-flake-inputs = {
    when = {
      hour = 1;
      minute = 0;
    };
    # nixbot mounts a pushable clone of the effect's commit at
    # $NIXBOT_EFFECT_CHECKOUT, which is also the working directory.
    outputs.effects.update-flake-inputs = mkEffect {
      name = "effect-update-flake-inputs";
      checkout = true;
      inputs = [
        pkgs.git
        pkgs.nix
      ];
      secretsMap.git.type = "GitToken";
      effectScript = ''
        set -euo pipefail
        token=$(jq -re '.git.data.token' "$HERCULES_CI_SECRETS_JSON")
        export FORGE_TOKEN="$token"
        export GITHUB_TOKEN="$token"
        export NIX_CONFIG="experimental-features = nix-command flakes
        access-tokens = github.com=$token"

        git config --global user.name 'fosskar[bot]'
        git config --global user.email '300917551+fosskar[bot]@users.noreply.github.com'

        git config remote.origin.promisor true
        git config remote.origin.partialclonefilter blob:none

        nix run "github:fosskar/nixfiles#updater-flake-inputs"
      '';
    };
  };
}

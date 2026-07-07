# nixbot scheduled effects. Tokens come from nixbot at runtime (GitToken +
# the github-api secret wildcarded to fosskar/* in nixfiles' nixbot module).
{ pkgs }:
_args: {
  onSchedule.update-flake-inputs = {
    when = {
      hour = 5;
      minute = 0;
    };
    outputs.effects.update-flake-inputs =
      pkgs.runCommand "effect-update-flake-inputs"
        {
          nativeBuildInputs = [
            pkgs.cacert
            pkgs.git
            pkgs.jq
            pkgs.nix
          ];
          secretsMap = builtins.toJSON {
            git.type = "GitToken";
            github = "github-api";
          };
          HOME = "/build";
        }
        ''
          set -euo pipefail
          token=$(jq -re '.git.data.token' "$HERCULES_CI_SECRETS_JSON")
          export FORGE_TOKEN="$token"
          github_token=$(jq -re '.github.data.token' "$HERCULES_CI_SECRETS_JSON")
          export GITHUB_TOKEN="$github_token"
          export NIX_CONFIG="experimental-features = nix-command flakes
          access-tokens = github.com=$github_token"

          git config --global user.name nixbot
          git config --global user.email nixbot@nx3.eu
          git config --global safe.directory '*'

          git clone --depth 1 --progress \
            "https://oauth2:$token@codeberg.org/fosskar/ssync.git" repo
          cd repo

          nix run "git+https://codeberg.org/fosskar/nixfiles?shallow=1#updater-flake-inputs"
        '';
  };
}

{
  description = "formigueiro — the fleet update-swarm daemon (shadow-first)";

  # Canonical pleme-io Rust-tool consumer flake. substrate.rust.tool pre-binds
  # nixpkgs / crate2nix / flake-utils / fenix / devenv / gen — every dependency the
  # build kit needs — so a substrate bump propagates fleet-wide without touching this
  # file. toolName (formigueiro) + repo are read from the typed
  # `flake_metadata.formigueiro` in Cargo.build-spec.json.
  inputs.substrate.url = "github:pleme-io/substrate";

  outputs = { substrate, ... }: substrate.rust.tool {
    src = ./.;
    member = "formigueiro"; # the deployable bin; the other 6 members are libraries
    module = {
      description = "formigueiro — the fleet update-swarm daemon (shadow-first)";
    };
  };
}

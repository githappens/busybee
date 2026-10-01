{ pkgs }:
let
  version = "1.24.1";
  archives = {
    aarch64-darwin = { name = "darwin_arm64"; sha256 = "2af4924f80def5b814950ac07fc8261858e9bb1543b395747231fa412627be0b"; };
    x86_64-darwin = { name = "darwin_amd64"; sha256 = "f21e51d492ae2c15acd383c05dd32a3bcaa20dd75f4eac85423759339f3970b0"; };
    aarch64-linux = { name = "linux_arm64"; sha256 = "1c48c30bdfa10858c65c226711cc9a0e68e3bbe3a2d52475af020762521908cb"; };
    x86_64-linux = { name = "linux_amd64"; sha256 = "b943a50d389f1883b98ccd590884d5fb1e045e80dddea5fe62df6e15c249dffe"; };
  };
  archive = archives.${pkgs.stdenv.hostPlatform.system};
in
pkgs.stdenvNoCC.mkDerivation {
  pname = "sortie";
  inherit version;
  src = pkgs.fetchurl {
    url = "https://github.com/sortie-ai/sortie/releases/download/v${version}/sortie_${version}_${archive.name}.tar.gz";
    inherit (archive) sha256;
  };
  sourceRoot = ".";
  dontBuild = true;
  installPhase = ''
    runHook preInstall
    install -Dm755 sortie "$out/bin/sortie"
    runHook postInstall
  '';
  doInstallCheck = true;
  installCheckPhase = ''
    "$out/bin/sortie" --version
  '';
}

# Mirror of the formula .github/workflows/release.yml regenerates on every v*
# tag from the release's build artifacts — it is uploaded as a workflow
# artifact, never published from here. Refresh this copy on each release from
# the release's .sha256 assets.
class Atim < Formula
  desc "AI Agent Through IM"
  homepage "https://github.com/zitsen/atim"
  version "0.7.0"
  license "MIT"

  on_macos do
    odie "atim does not support macOS yet. Please use Linux."
  end

  on_linux do
    if Hardware::CPU.arm? && Hardware::CPU.is_64_bit?
      url "https://github.com/zitsen/atim/releases/download/v#{version}/atim-aarch64-unknown-linux-musl.tar.gz"
      sha256 "482e7799c14522238135365ae00ee7946ff6967e3a038c9d9f43b8ecce1ea2c8"
    else
      url "https://github.com/zitsen/atim/releases/download/v#{version}/atim-x86_64-unknown-linux-musl.tar.gz"
      sha256 "e47be78bccc5a29ca8313f7d8a89afc7c635f1a1b5011ed018f0394b30e61bce"
    end
  end

  def install
    bin.install "atim"
  end

  test do
    assert_match "atim", shell_output("#{bin}/atim --help")
  end
end
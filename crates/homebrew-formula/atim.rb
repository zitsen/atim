class Atim < Formula
  desc "AI Agent Through IM"
  homepage "https://github.com/zitsen/atim"
  version "0.2.0"
  license "MIT"

  on_macos do
    odie "atim does not support macOS yet. Please use Linux."
  end

  on_linux do
    if Hardware::CPU.arm? && Hardware::CPU.is_64_bit?
      url "https://github.com/zitsen/atim/releases/download/v#{version}/atim-aarch64-unknown-linux-musl.tar.gz"
      sha256 "3d753cf50e5f3daaff0bd6bc4be11c53789aa155fe0d272dbc4527b141338765"
    else
      url "https://github.com/zitsen/atim/releases/download/v#{version}/atim-x86_64-unknown-linux-musl.tar.gz"
      sha256 "2f01903ab10a31f23f0f7f0be699de650d0d472538612c4bba86d4542d673acf"
    end
  end

  def install
    bin.install "atim"
  end

  test do
    assert_match "atim", shell_output("\#{bin}/atim --help")
  end
end

# Homebrew formula template. sha256 values are filled after the first v* GitHub Release.
class Mushroomdb < Formula
  desc "Embedded property-graph database with incremental linking rules"
  homepage "https://github.com/MatthewSherlin/graph-db"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/MatthewSherlin/graph-db/releases/download/v0.1.0/mushroomdb-v0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "PUT_SHA256_AFTER_FIRST_RELEASE"
    end
    on_intel do
      url "https://github.com/MatthewSherlin/graph-db/releases/download/v0.1.0/mushroomdb-v0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "PUT_SHA256_AFTER_FIRST_RELEASE"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/MatthewSherlin/graph-db/releases/download/v0.1.0/mushroomdb-v0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "PUT_SHA256_AFTER_FIRST_RELEASE"
    end
    on_intel do
      url "https://github.com/MatthewSherlin/graph-db/releases/download/v0.1.0/mushroomdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "PUT_SHA256_AFTER_FIRST_RELEASE"
    end
  end

  def install
    bin.install "mushroomdb"
  end

  test do
    output = shell_output("#{bin}/mushroomdb --help")
    assert_match "mushroomdb", output
  end
end

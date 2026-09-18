# Homebrew formula template for qld, built from source.
#
# To publish: copy to a tap (e.g. KarpelesLab/homebrew-tap, Formula/qld.rb),
# set `url` to the release tag's source tarball and `sha256` to its
# checksum (`curl -L URL | shasum -a 256`), then `brew audit --new qld`
# and `brew test qld`. See packaging/README.md.
class Qld < Formula
  desc "Fast, parallel linker compatible with GNU ld, gold, lld, mold and ld64"
  homepage "https://github.com/KarpelesLab/qld"
  url "https://github.com/KarpelesLab/qld/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  head "https://github.com/KarpelesLab/qld.git", branch: "master"

  depends_on "rust" => :build

  def install
    # The release profile keeps line tables for profiling; not needed here.
    ENV["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
    system "cargo", "install", *std_cargo_args

    # clang -fuse-ld=qld looks for ld64.qld when targeting macOS and for
    # ld.qld otherwise.
    bin.install_symlink "qld" => "ld64.qld"
    bin.install_symlink "qld" => "ld.qld"
    # gcc -B#{opt_libexec}/qld on Linux. Not on macOS: a linker run as `ld`
    # takes the GNU command line, and gcc there drives ld64.
    (libexec/"qld").install_symlink bin/"qld" => "ld" if OS.linux?
  end

  def caveats
    <<~EOS
      To link with qld:
        clang -fuse-ld=qld ...
        clang --ld-path=#{opt_bin}/#{OS.mac? ? "ld64.qld" : "ld.qld"} ...
    EOS
  end

  test do
    assert_match "qld #{version}", shell_output("#{bin}/qld --version")
    # ld64.qld takes the ld64 command line.
    system bin/"ld64.qld", "-arch", "arm64", "-v"

    (testpath/"hello.c").write <<~C
      #include <stdio.h>
      int main(void) { puts("hello from qld"); return 0; }
    C
    if OS.mac?
      # ENV.cc is Apple clang.
      system ENV.cc, "--ld-path=#{bin}/ld64.qld", "hello.c", "-o", "hello"
    else
      # ENV.cc is gcc, which has no -fuse-ld=qld or --ld-path.
      system ENV.cc, "-B#{libexec}/qld", "hello.c", "-o", "hello"
      assert_match "Linker: qld", File.binread("hello")
    end
    assert_equal "hello from qld\n", shell_output("./hello")
  end
end

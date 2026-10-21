class Pine < Formula
  desc "Capture AI traffic locally and sync sessions to an organization server"
  homepage "https://github.com/leanmcp-community/pine"
  license "MIT"
  head "https://github.com/leanmcp-community/pine.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args(path: buildpath)
    bin.install_symlink "pine" => "pn" # short alias
  end

  service do
    run [opt_bin/"pine", "cloud", "sync"]
    keep_alive successful_exit: false
    log_path var/"log/pine-sync.log"
    error_log_path var/"log/pine-sync.log"
  end

  def caveats
    <<~EOS
      Configure your server and upload key before starting the uploader:
        export PINE_UPLOAD_KEY='your-key'
        pine cloud configure --endpoint https://your-server
        unset PINE_UPLOAD_KEY
        brew services start leanmcp-community/pine/pine

      Run without sudo to use your own ~/.pine captures and credentials.
      Stop any older Cargo-installed uploader service before starting this one.
      Check `which pine` if an older Cargo binary shadows this installation.
    EOS
  end

  test do
    assert_match "cloud", shell_output("#{bin}/pine --help")
    assert_match "sync", shell_output("#{bin}/pine cloud --help")
    assert_match "already stopped", shell_output("PINE_HOME=#{testpath}/capture #{bin}/pine cloud stop")
  end
end

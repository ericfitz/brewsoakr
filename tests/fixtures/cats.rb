# Documentation: https://docs.brew.sh/Formula-Cookbook
#                https://rubydoc.brew.sh/Formula
# PLEASE REMOVE ALL GENERATED COMMENTS BEFORE SUBMITTING YOUR PULL REQUEST!
class Cats < Formula
  desc "CATS is a REST APIs fuzzer and negative testing tool for OpenAPI endpoints. CATS automatically  generates, runs and reports tests with minimum configuration and no coding effort. Tests are self-healing and do not require maintenance."
  homepage "https://endava.github.io/cats/"

  if OS.mac?
      if Hardware::CPU.intel?
        url "https://github.com/Endava/cats/releases/download/cats-14.0.0/cats_macos-15_amd64_14.0.0.tar.gz"
        sha256 "44de442d123f2bfa421b5cf78e5fa529d3fe6b8756d15b7cd2a9ab4953a95b2d"
      elsif Hardware::CPU.arm?
        url "https://github.com/Endava/cats/releases/download/cats-14.0.0/cats_macos_arm64_14.0.0.tar.gz"
        sha256 "89f5ac4b179ec8b760c26e9293cdb5f403acda8152618be81a4d54289653b788"
      end
  elsif OS.linux?
      if Hardware::CPU.intel?
        url "https://github.com/Endava/cats/releases/download/cats-14.0.0/cats_linux_amd64_14.0.0.tar.gz"
        sha256 "5910efdaeaa0e5ea486c54722a7a6abbed38e2bd673a803d7726c65c173af040"
      elsif Hardware::CPU.arm?
        url "https://github.com/Endava/cats/releases/download/cats-14.0.0/cats_linux_arm64_14.0.0.tar.gz"
        sha256 "73480a78da8c465351793a7f0d309d9ebadbe8bfb7c32a355d53df45d940771d"
      end
  else
    odie "Please use the uberjar version: https://github.com/Endava/cats/releases/download/cats-14.0.0/cats_uberjar_14.0.0.tar.gz"
  end

  license "Apache-2.0"

  # depends_on "cmake" => :build

  def install
    # ENV.deparallelize  # if your formula fails when building in parallel
    # Remove unrecognized options if warned by configure
    # https://rubydoc.brew.sh/Formula.html#std_configure_args-instance_method
    # system "./configure", *std_configure_args, "--disable-silent-rules"
    # system "cmake", "-S", ".", "-B", "build", *std_cmake_args
    bin.install 'cats'
  end

  test do
    # `test do` will create, run in and delete a temporary directory.
    #
    # This test will fail and we won't accept that! For Homebrew/homebrew-core
    # this will need to be a test that verifies the functionality of the
    # software. Run the test with `brew test cats`. Options passed
    # to `brew install` such as `--HEAD` also need to be provided to `brew test`.
    #
    # The installed folder is not in the path, so use the entire path to any
    # executables being tested: `system "#{bin}/program", "do", "something"`.
    system "false"
  end
end

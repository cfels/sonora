{ pkgs }:
let
  gst = pkgs.gst_all_1;
  libraries = with pkgs; [
    fontconfig freetype libxkbcommon wayland libGL vulkan-loader
    libX11 libXcursor libXi libXrandr libxcb
  ];
  plugins = with gst; [
    gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav
  ];
in
pkgs.mkShell {
  packages = with pkgs; [
    cargo rustc rustfmt clippy pkg-config fontconfig gst.gstreamer gst.gst-plugins-base
  ] ++ libraries ++ plugins;
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libraries;
  GST_PLUGIN_SYSTEM_PATH_1_0 = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" plugins;
}

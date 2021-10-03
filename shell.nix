with import <nixpkgs> { }; 

runCommand "dummy" {
    buildInputs = [ cargo rustup gcc alsa-lib automake autoconf perl pkgconfig ];
} ""

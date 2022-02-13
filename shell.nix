with import <nixpkgs> { }; 

runCommand "dummy" {
    buildInputs = [ rustup gcc alsa-lib automake autoconf perl pkgconfig ];
} ""

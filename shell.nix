with import <nixpkgs> { }; 

runCommand "dummy" {
    buildInputs = [ rustup gcc alsa-lib cmake libopus automake autoconf perl pkg-config ];
} ""

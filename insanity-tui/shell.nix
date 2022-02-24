with import <nixpkgs> { }; 

runCommand "dummy" {
    buildInputs = [ cargo rustup gcc automake autoconf perl pkgconfig ];
} ""

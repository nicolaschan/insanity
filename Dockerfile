FROM rust:latest

RUN apt-get update && apt-get install -y libasound2-dev

WORKDIR /usr/src/insanity
COPY . .

RUN cargo install --path .

ENTRYPOINT ["insanity"]

FROM --platform=amd64 rust:1.65
RUN apt-get update && apt-get install -y libunwind-dev

WORKDIR /usr/src
COPY . .
RUN find . -name "*.rs" -not -name "lib.rs" -not -name "main.rs" -type f -delete
RUN cargo fetch
COPY . .
RUN cargo build --release

ENTRYPOINT [ "target/release/hermit" ]
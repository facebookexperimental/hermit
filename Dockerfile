FROM ubuntu:latest
WORKDIR /usr/src/hermit

RUN apt-get update
RUN apt-get install -y \
        build-essential \
        curl \
        libunwind-dev

RUN curl https://sh.rustup.rs -sSf | bash -s -- -y
RUN echo 'source $HOME/.cargo/env' >> $HOME/.bashrc

COPY . .
RUN cargo update
RUN cargo build

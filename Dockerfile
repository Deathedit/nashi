# --- nashi itself -----------------------------------------------------------
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked

# --- zarchive + extract-xiso (not packaged by Debian) ----------------------
FROM debian:trixie-slim AS tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential cmake pkg-config git ca-certificates libzstd-dev \
 && rm -rf /var/lib/apt/lists/*
ARG ZARCHIVE_REF=v0.1.2
ARG EXTRACT_XISO_REF=build-202609111233
RUN git clone --depth 1 --branch "$ZARCHIVE_REF" https://github.com/Exzap/ZArchive /tmp/zarchive \
 && cmake -S /tmp/zarchive -B /tmp/zarchive/build -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=/opt/tools \
 && cmake --build /tmp/zarchive/build -j"$(nproc)" \
 && cmake --install /tmp/zarchive/build
RUN git clone --depth 1 --branch "$EXTRACT_XISO_REF" https://github.com/XboxDev/extract-xiso /tmp/extract-xiso \
 && cmake -S /tmp/extract-xiso -B /tmp/extract-xiso/build -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=/opt/tools \
 && cmake --build /tmp/extract-xiso/build -j"$(nproc)" \
 && cmake --install /tmp/extract-xiso/build

# --- runtime ------------------------------------------------------------------
FROM debian:trixie-slim
# mame-tools provides chdman
RUN apt-get update \
 && apt-get install -y --no-install-recommends mame-tools libzstd1 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --uid 1000 nashi \
 && mkdir /work && chown nashi:nashi /work
COPY --from=tools /opt/tools/bin/zarchive /opt/tools/bin/extract-xiso /usr/local/bin/
COPY --from=build /src/target/release/nashi /usr/local/bin/nashi
COPY web /app/web

ENV NASHI_ADDR=0.0.0.0:8080 \
    NASHI_WORK_DIR=/work \
    NASHI_WEB_DIR=/app/web
USER nashi
EXPOSE 8080
VOLUME /work
CMD ["nashi"]

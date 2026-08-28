# Stage 1: Build
FROM rust:trixie AS builder
RUN apt-get update && apt-get install -y --no-install-recommends libdbus-1-dev pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
# BuildKit cache mounts keep the cargo registry and compiled dependencies
# across builds, so only changed crates are recompiled. The caches are not
# part of the image — they persist on the builder between runs.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --features k8s,otlp && \
    cp target/release/pali /usr/local/bin/pali

# Stage 2: Model download
# The candle embedding engine downloads model weights from HuggingFace Hub
# via the hf-hub crate on first use. We pre-download by running "warmup" in
# this stage so the runtime image ships with the model already on disk — no
# internet access required at pod startup, and cold-start latency is eliminated.
#
# HF_HOME controls where hf-hub caches downloaded models. We set it to an
# absolute path under the app user's home directory.
FROM debian:trixie-slim AS model
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1000 app
COPY --from=builder /usr/local/bin/pali /usr/local/bin/pali
USER app
ENV HF_HOME=/home/app/.cache/huggingface
RUN /usr/local/bin/pali warmup

# Stage 3: Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git libdbus-1-3 && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1000 pali
COPY --from=builder /usr/local/bin/pali /usr/local/bin/pali
# Copy the pre-warmed model cache from the model stage.
# --chown avoids a separate chown layer that would double the ~130 MB cache.
COPY --from=model --chown=pali:pali /home/app/.cache/huggingface /home/pali/.cache/huggingface
USER pali
WORKDIR /home/pali
ENV PALI_BIND=0.0.0.0:8080
ENV PALI_REPO_PATH=/data/repo
# Pin HF_HOME so hf-hub finds the pre-warmed model files regardless of CWD.
ENV HF_HOME=/home/pali/.cache/huggingface
EXPOSE 8080
ENTRYPOINT ["pali"]
CMD ["serve"]

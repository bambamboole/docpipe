FROM rust:1.98-slim-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY profiles ./profiles
RUN cargo build --release --locked

############################################
# OCR runtime: PDFium, ONNX Runtime and the PP-OCRv6 Small models, pinned to
# the versions pdf-inspector validates and verified by SHA-256.
############################################
FROM debian:bookworm-slim AS ocr-runtime

ARG TARGETARCH
ARG PDFIUM_RELEASE=native-v7988
ARG PDFIUM_AMD64_SHA256=6248189e07bbc33cdeb31976c539a88614307c8a19f3276dbd018efbe5b4a2a2
ARG PDFIUM_ARM64_SHA256=fb67e132abf9a816194bb40cbe1c9e1f6e3bfcaa0b0ef645bf2e6b9f155c8a73
ARG ORT_VERSION=1.27.0
ARG ORT_AMD64_SHA256=547e40a48f1fe73e3f812d7c88a948612c23f896b91e4e2ee1e232d7b468246f
ARG ORT_ARM64_SHA256=3e4d83ac06924a32a07b6d7f91ce6f852876153fc0bbdf931bf517a140bfbe48
ARG MODEL_REVISION=oar-ocr-v0.7.0

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /download
RUN case "$TARGETARCH" in \
        amd64) pdfium_arch=x64; ort_arch=x64; pdfium_sha="$PDFIUM_AMD64_SHA256"; ort_sha="$ORT_AMD64_SHA256" ;; \
        arm64) pdfium_arch=arm64; ort_arch=aarch64; pdfium_sha="$PDFIUM_ARM64_SHA256"; ort_sha="$ORT_ARM64_SHA256" ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSLo pdfium.tgz "https://github.com/firecrawl/pdfium-rs/releases/download/${PDFIUM_RELEASE}/firecrawl-pdfium-linux-${pdfium_arch}.tgz" \
    && echo "${pdfium_sha}  pdfium.tgz" | sha256sum -c - \
    && mkdir pdfium && tar -xzf pdfium.tgz -C pdfium \
    && curl -fsSLo ort.tgz "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ort_arch}-${ORT_VERSION}.tgz" \
    && echo "${ort_sha}  ort.tgz" | sha256sum -c - \
    && tar -xzf ort.tgz \
    && mkdir -p /opt/ocr/lib /opt/ocr/models \
    && cp "$(find pdfium -name libpdfium.so | head -1)" /opt/ocr/lib/libpdfium.so \
    && cp -L "onnxruntime-linux-${ort_arch}-${ORT_VERSION}/lib/libonnxruntime.so" /opt/ocr/lib/libonnxruntime.so

WORKDIR /opt/ocr/models
RUN base="https://github.com/GreatV/oar-ocr/releases/download/v0.7.0" \
    && curl -fsSLO "${base}/pp-ocrv6_small_det.onnx" \
    && curl -fsSLO "${base}/pp-ocrv6_small_rec.onnx" \
    && curl -fsSLO "${base}/ppocrv6_dict.txt" \
    && printf '%s\n' \
        "d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e  pp-ocrv6_small_det.onnx" \
        "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634  pp-ocrv6_small_rec.onnx" \
        "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d  ppocrv6_dict.txt" \
        | sha256sum -c -

############################################
# Corpus OCR test: the regular cargo test job runs without native OCR assets.
############################################
FROM build AS ocr-test

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends libstdc++6 libgomp1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ocr-runtime /opt/ocr /opt/ocr
COPY tests ./tests

ENV PDFIUM_LIB_PATH=/opt/ocr/lib/libpdfium.so \
    ORT_DYLIB_PATH=/opt/ocr/lib/libonnxruntime.so \
    DOCPIPE_OCR_MODEL_DIR=/opt/ocr/models

RUN cargo test --release --locked --test sinabis_corpus -- --ignored --nocapture

############################################
# Production
############################################
FROM debian:bookworm-slim

LABEL org.opencontainers.image.source="https://github.com/bambamboole/docpipe" \
      org.opencontainers.image.description="Document analysis service for invoices and quotes" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends ca-certificates curl libstdc++6 libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 docpipe

COPY --from=ocr-runtime /opt/ocr /opt/ocr
COPY --from=build /src/target/release/docpipe /usr/local/bin/docpipe

# Models are never downloaded at runtime: the directory is complete.
ENV PORT=8080 \
    PDFIUM_LIB_PATH=/opt/ocr/lib/libpdfium.so \
    ORT_DYLIB_PATH=/opt/ocr/lib/libonnxruntime.so \
    DOCPIPE_OCR_MODEL_DIR=/opt/ocr/models

USER docpipe
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fs http://localhost:8080/health || exit 1

CMD ["docpipe"]

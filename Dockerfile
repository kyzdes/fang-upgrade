# syntax=docker/dockerfile:1
FROM rust:1.91-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y pkg-config libssl-dev perl make && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
COPY agents ./agents
COPY packages ./packages
# Optional build args for dev environments to speed up compilation
# Example: docker build --build-arg LTO=false --build-arg CODEGEN_UNITS=16 .
ARG LTO=true
ARG CODEGEN_UNITS=1
ENV CARGO_PROFILE_RELEASE_LTO=${LTO} \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CODEGEN_UNITS}

# Кто эта сборка. Без этих трёх `/api/version` отдаёт
# {"version":"0.6.9","git_sha":"unknown","build_date":"dev"} — то же, что ваниль,
# то есть по API отличить форк от стока нельзя вообще. Проверено на живом проде.
#
# Передавать так:
#   docker build --build-arg GIT_SHA=$(git rev-parse --short HEAD) \
#                --build-arg GIT_DESCRIBE=$(git describe --tags --always --dirty) \
#                --build-arg BUILD_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ) .
#
# Пустые значения оставлены допустимыми намеренно: сборка без них не должна падать,
# но и врать не будет — `/api/version` покажет "unknown", как и раньше.
ARG GIT_SHA=""
ARG GIT_DESCRIBE=""
ARG BUILD_DATE=""
ENV GIT_SHA=${GIT_SHA} \
    GIT_DESCRIBE=${GIT_DESCRIBE} \
    BUILD_DATE=${BUILD_DATE}
# Cache the registry, the git checkouts and target/ across builds. Without this a
# one-line change recompiles all 13 crates from scratch: ~9 minutes on 4 cores,
# which is the whole cost of iterating on a patch.
#
# sharing=locked, not the default "shared": two concurrent builds writing the same
# target/ corrupt each other's incremental state. Serialising them is cheaper than
# debugging the result.
#
# The binary is copied out inside this RUN on purpose. A cache mount is not part of
# the layer, so `COPY --from=builder /build/target/...` in the next stage would find
# nothing there — and would do it silently, producing an image whose openfang is
# missing or stale. Hence /openfang, which is a real layer file.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --bin openfang \
    && cp target/release/openfang /openfang

FROM rust:1.91-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    python3 \
    python3-pip \
    python3-venv \
    nodejs \
    npm \
    && rm -rf /var/lib/apt/lists/*

# yt-dlp для конвейера youtube-insights. Нужен агенту `youtube-insights`, который
# зовёт /data/workspaces/youtube-insights/bin/ytwatch.py через shell_exec.
#
# ПОЧЕМУ В ОБРАЗЕ, А НЕ pip В ЖИВОЙ КОНТЕЙНЕР. Именно так он и стоял — в
# записываемом слое — и исчез при первой же замене контейнера на образ из CI
# 2026-08-23. Скрипт уцелел (он в томе /data), бинарь нет. Тот же класс, что
# «hand vanishes after docker restart»: всё, что не в образе и не в томе,
# живёт до следующего `up -d`.
#
# ПОЧЕМУ pip, А НЕ apt. Пакет из Debian отстаёт, а YouTube ломает старые версии
# за недели.
#
# ПОЧЕМУ ЗАКРЕПЛЁННАЯ ВЕРСИЯ. Плавающая даёт разный образ на одном коммите — это
# ровно то, на чём 23 августа встала сборка (rust 1.88 против объявленного 1.91).
# Здесь цена та же: воспроизводимость важнее свежести, а свежесть достигается
# осознанным подъёмом одной строки.
#
# КОГДА ПОДНИМАТЬ. Когда `ytwatch.py fetch` начнёт возвращать ошибку разбора
# вместо субтитров — это YouTube сменил формат, и нужна новая версия.
#
# ffmpeg НЕ нужен: конвейер тянет субтитры (--write-auto-subs --sub-format json3
# --skip-download), а не медиа. Проверено: вхождений ffmpeg в ytwatch.py — ноль.
ARG YTDLP_VERSION=2026.8.19
RUN pip3 install --break-system-packages --no-cache-dir "yt-dlp==${YTDLP_VERSION}" \
    && yt-dlp --version

COPY --from=builder /openfang /usr/local/bin/openfang
COPY --from=builder /build/agents /opt/openfang/agents
EXPOSE 4200
VOLUME /data
ENV OPENFANG_HOME=/data
ENTRYPOINT ["openfang"]
CMD ["start"]

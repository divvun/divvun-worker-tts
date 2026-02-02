FROM debian:trixie-slim

RUN apt-get update && apt-get install -y \
    libicu76 \
    libsqlite3-0 \
    libboost-system1.83.0 \
    libboost-filesystem1.83.0 \
    libboost-program-options1.83.0 \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY ./divvun-worker-tts /usr/local/bin/

WORKDIR /app
EXPOSE 4000
ENV ADDRESS="tcp://0.0.0.0:4000"

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:4000/health || exit 1

ENTRYPOINT ["divvun-worker-tts"]

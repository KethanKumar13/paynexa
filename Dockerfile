FROM rust:1.95-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release --locked -p api

FROM debian:trixie-slim
RUN useradd --system --uid 10001 app
COPY --from=build /app/target/release/api /usr/local/bin/api
USER app
CMD ["/usr/local/bin/api"]

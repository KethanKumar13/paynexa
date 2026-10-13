FROM rust:1.95-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release --locked -p api -p mock-psp

FROM debian:trixie-slim
RUN useradd --system --uid 10001 app
COPY --from=build /app/target/release/api /usr/local/bin/api
COPY --from=build /app/target/release/mock-psp /usr/local/bin/mock-psp
USER app
CMD ["/usr/local/bin/api"]

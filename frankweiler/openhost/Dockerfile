# Frankweiler — Open Host container (v0 stub)
# Builds the axum HTTP backend and bakes in the static UI bundle.

FROM rust:1.83 AS backend-build
WORKDIR /src
COPY frankweiler/backend ./frankweiler/backend
COPY schemas ./schemas
WORKDIR /src/frankweiler/backend
RUN cargo build --release -p frankweiler-http

FROM node:20 AS ui-build
WORKDIR /src/frankweiler/ui
COPY frankweiler/ui/package.json frankweiler/ui/pnpm-lock.yaml ./
RUN corepack enable && pnpm install --frozen-lockfile
COPY frankweiler/ui ./
RUN pnpm build

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=backend-build /src/frankweiler/backend/target/release/frankweiler-http /app/frankweiler-http
COPY --from=ui-build /src/frankweiler/ui/dist /app/dist
EXPOSE 8731
CMD ["/app/frankweiler-http"]

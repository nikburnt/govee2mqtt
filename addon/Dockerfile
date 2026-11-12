ARG BUILD_FROM
ARG GOVEE_VERSION=2026.11.12-local1
FROM ghcr.io/nikburnt/govee2mqtt:${GOVEE_VERSION} AS govee2mqtt
FROM $BUILD_FROM
COPY run.sh /run.sh
COPY --from=govee2mqtt /app/govee /app/
COPY --from=govee2mqtt /app/assets /app/assets/
COPY --from=govee2mqtt /app/AmazonRootCA1.pem /app/
CMD [ "/run.sh" ]

FROM alpine:latest
ARG TARGETARCH
COPY ./packages/shadowquic-${TARGETARCH} /usr/bin/shadowquic
RUN chmod +x /usr/bin/shadowquic
WORKDIR /root

ENTRYPOINT [ "/usr/bin/shadowquic" ]
CMD [ "-c", "/etc/shadowquic/config.yaml" ]
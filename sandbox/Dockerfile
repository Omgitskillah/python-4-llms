FROM python:3.14.8-slim-bookworm@sha256:c8137f4c460908c8763f281c8f22c431eb5c538514ba9553fc3a89c06b7cfb88

COPY --chown=0:0 --chmod=0444 sandbox/runner.py /opt/runner.py
USER 1000:1000
WORKDIR /tmp
ENTRYPOINT ["/usr/local/bin/python", "-I", "-B", "-u", "/opt/runner.py"]

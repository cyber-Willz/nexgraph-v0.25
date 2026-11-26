#!/bin/sh
# TEST-ONLY certificates (private key is committed on purpose; never use these outside tests).
set -e
cd "$(dirname "$0")"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ca.key -out ca.pem -days 36500 \
  -subj "/CN=nexgraph test CA" -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout server.key -out server.csr -subj "/CN=pg.test"
printf "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:pg.test,DNS:localhost,IP:127.0.0.1\n" > ext.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 36500 -extfile ext.cnf
# an unrelated CA, to prove that a wrong trust anchor is rejected
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout other_ca.key -out other_ca.pem -days 36500 -subj "/CN=some other CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign"
rm -f server.csr ext.cnf ca.srl ca.key other_ca.key

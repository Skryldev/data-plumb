<!-- title: Go Client category: integration audience: developer last_updated: 2026-11-10 -->

# Go Client

The gRPC API is defined in package `file_storage.v1` under `proto/file_storage/v1/`, with `go_package = "github.com/your-org/file-storage/api/v1;v1"` already declared in the protos. Generate the Go stubs once, then implement upload as a client stream of 1 MiB pieces and download as a server-streaming loop. Message shapes and streaming semantics are documented in [gRPC API](../api/grpc.md).

## Overview

A Go client needs four things: generated stubs, a channel (plaintext or TLS), the bearer token from `AuthService/Login` attached as `authorization` metadata, and the header-first streaming convention — the first `UploadChunk` message carries the header, every subsequent one carries bytes. Errors are inspected with `status.FromError` and the standard `codes` package.

## How it works

Generate stubs with either toolchain from the repository root:

```bash
# buf (buf.yaml lives next to the protos; buf.gen.yaml below)
buf generate

# or plain protoc
protoc -I proto \
  --go_out=gen/go --go_opt=paths=source_relative \
  --go-grpc_out=gen/go --go-grpc_opt=paths=source_relative \
  proto/file_storage/v1/common.proto \
  proto/file_storage/v1/auth.proto \
  proto/file_storage/v1/file.proto \
  proto/file_storage/v1/admin.proto
```

```yaml
# buf.gen.yaml
version: v1
plugins:
  - plugin: go
    out: gen/go
    opt: paths=source_relative
  - plugin: go-grpc
    out: gen/go
    opt: paths=source_relative
```

```bash
go get google.golang.org/grpc
```

## Configuration

Dependencies for the example below:

```bash
go get google.golang.org/grpc@latest google.golang.org/protobuf@latest
```

If your server runs behind `server.grpc_tls`, load the CA into a `x509.CertPool` and dial with `credentials.NewClientTLSFromCert(pool, "")` — the name you dial must match a SAN in the certificate.

## Examples

A complete client: dial, login, upload a file from an `io.Reader` in 1 MiB chunks, download it back to disk, and map errors.

```go
package main

import (
	"context"
	"io"
	"log"
	"os"

	pb "github.com/your-org/file-storage/gen/go/file_storage/v1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

const pieceSize = 1 << 20 // 1 MiB

func dial() (*grpc.ClientConn, error) {
	// Plaintext (internal). For TLS: credentials.NewClientTLSFromCert(pool, "").
	return grpc.NewClient("localhost:9090", grpc.WithTransportCredentials(insecure.NewCredentials()))
}

func main() {
	conn, err := dial()
	if err != nil {
		log.Fatal(err)
	}
	defer conn.Close()
	ctx := context.Background()

	// 1. Login; token travels as authorization metadata from here on.
	auth := pb.NewAuthServiceClient(conn)
	lr, err := auth.Login(ctx, &pb.LoginRequest{Username: "alice", Password: "alice123"})
	if err != nil {
		log.Fatal(err)
	}
	ctx = metadata.AppendToOutgoingContext(ctx, "authorization", "Bearer "+lr.AccessToken)
	files := pb.NewFileServiceClient(conn)

	// 2. Upload: header message first, then 1 MiB data pieces.
	src, _ := os.Open("report.pdf")
	defer src.Close()
	info, _ := src.Stat()
	stream, err := files.Upload(ctx)
	if err != nil {
		log.Fatal(err)
	}
	if err := stream.Send(&pb.UploadChunk{Payload: &pb.UploadChunk_Header{
		Header: &pb.UploadHeader{Name: "report.pdf", MimeType: "application/pdf",
			ExpectedSize: uint64(info.Size()), Tags: []string{"q1"}},
	}}); err != nil {
		log.Fatal(err)
	}
	buf := make([]byte, pieceSize)
	for {
		n, rerr := src.Read(buf)
		if n > 0 {
			if serr := stream.Send(&pb.UploadChunk{Payload: &pb.UploadChunk_Data{Data: buf[:n]}}); serr != nil {
				log.Fatal(serr)
			}
		}
		if rerr == io.EOF {
			break
		}
		if rerr != nil {
			log.Fatal(rerr)
		}
	}
	resp, err := stream.CloseAndRecv()
	if err != nil {
		log.Fatal(err)
	}
	id := resp.Manifest.FileId
	log.Printf("uploaded %s (dedup_chunks=%d stored=%d)", id, resp.DedupChunks, resp.StoredBytes)

	// 3. Download: server streams header then ordered data.
	dl, err := files.Download(ctx, &pb.DownloadRequest{FileId: id})
	if err != nil {
		log.Fatal(err)
	}
	dst, _ := os.Create("copy.pdf")
	defer dst.Close()
	for {
		msg, rerr := dl.Recv()
		if rerr == io.EOF {
			break
		}
		if rerr != nil {
			log.Fatal(rerr)
		}
		switch p := msg.Payload.(type) {
		case *pb.DownloadChunk_Header:
			log.Printf("total_size=%d mime=%s", p.Header.TotalSize, p.Header.MimeType)
		case *pb.DownloadChunk_Data:
			if _, werr := dst.Write(p.Data); werr != nil {
				log.Fatal(werr)
			}
		}
	}
	// hdr.OriginalHash (32-byte BLAKE3 of the full plaintext) is available on the
	// header message for end-to-end verification; verify with golang.org/x/crypto/blake2b
	// or your preferred BLAKE3 implementation.

	// 4. Soft delete; NOT_FOUND on an unknown id is expected and safe to ignore.
	if _, err := files.DeleteFile(ctx, &pb.DeleteFileRequest{FileId: id, Purge: false}); err != nil {
		if st, ok := status.FromError(err); ok && st.Code() == codes.NotFound {
			log.Println("already gone")
		} else {
			log.Fatal(err)
		}
	}
}
```

### grpcurl interop check

Before writing Go, confirm reachability and shapes from the shell — this catches port, TLS and metadata mistakes instantly:

```bash
grpcurl -plaintext localhost:9090 list                      # reflection must list 4 services + health
grpcurl -plaintext -d '{"username":"alice","password":"alice123"}' \
  localhost:9090 file_storage.v1.AuthService/Login
grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -d '{"file_id": "<id>"}' localhost:9090 file_storage.v1.FileService/GetManifest
```

## Usage notes

- Ranges: `DownloadRequest.Start` is inclusive, `End` **exclusive** (Go slice semantics); out-of-range `start >= size` yields `OUT_OF_RANGE`.
- Chunk pieces are arbitrary sizes — 1 MiB matches the server's framing guidance and the Rust SDK; pieces ≤ 64 KiB per message arrive on `PullChunk` only.
- `PermissionDenied` on another tenant's file is by design; use an admin account for cross-tenant work.
- For production, wrap unary calls with your own retry on `codes.Unavailable` only.

## See also

- [gRPC API](../api/grpc.md) — every RPC and message field.
- [Authentication](../api/authentication.md) — token TTLs, refresh rotation, roles.
- [Errors](../api/errors.md) — gRPC codes and their REST equivalents.
- [TLS](../deployment/tls.md) — enabling `server.grpc_tls` and mTLS.
- [CLI Recipes](./cli.md) — grpcurl patterns for debugging.
- [Rust Client](./rust-client.md) — the reference SDK implementation.

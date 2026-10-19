// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/netip"
	"net/url"
	"strconv"
	"strings"
	"sync"
)

const (
	maxHTTPURLBytes      = 8192
	maxHTTPHeaderBytes   = 32 * 1024
	maxHTTPMetadataBytes = 256 * 1024
	maxHTTPObjectBytes   = 64 * 1024 * 1024
	maxHTTPListBytes     = 1024 * 1024
	maxHTTPErrorBytes    = 64 * 1024
)

var (
	errHTTPArgument    = errors.New("HTTP_REQUEST_INVALID")
	errHTTPAuthority   = errors.New("HTTP_AUTHORITY_REFUSED")
	errHTTPLimit       = errors.New("HTTP_LIMIT_EXCEEDED")
	errHTTPBody        = errors.New("HTTP_BODY_SOURCE_FAILED")
	errHTTPRedirect    = errors.New("HTTP_REDIRECT_REFUSED")
	errHTTPAuth        = errors.New("HTTP_AUTHENTICATION_REFUSED")
	errHTTPNetwork     = errors.New("HTTP_NETWORK_FAILED")
	errHTTPUnsupported = errors.New("HTTP_PLATFORM_UNAVAILABLE")
	errHTTPProtocol    = errors.New("HTTP_RESPONSE_INVALID")
	errHTTPTLS         = errors.New("HTTP_TLS_FAILED")
)

// nativeHTTPTransport is private to the callable archive. It delegates every
// network request to the fixed Apple implementation; it never creates a Go
// HTTP transport, resolver, dialer, credential provider or replay worker.
type nativeHTTPTransport struct {
	ctx       context.Context
	authority string
}

type nativeHTTPRequest struct {
	URL     string              `json:"url"`
	Method  string              `json:"method"`
	Headers map[string][]string `json:"headers"`
}

type nativeHTTPReply struct {
	Status  int                 `json:"status"`
	Headers map[string][]string `json:"headers"`
	Body    []byte              `json:"-"`
}

func httpAuthority(value *url.URL) (string, error) {
	if value == nil || value.Scheme != "https" || value.Host == "" || value.User != nil || value.Opaque != "" || value.Fragment != "" {
		return "", errHTTPAuthority
	}
	parsed, parseErr := url.Parse("https://" + value.Host)
	if parseErr != nil || parsed.Host != value.Host || parsed.User != nil || parsed.Path != "" || parsed.RawQuery != "" || parsed.Fragment != "" {
		return "", errHTTPAuthority
	}
	host := value.Hostname()
	if host == "" || len(host) > 253 {
		return "", errHTTPAuthority
	}
	if _, err := netip.ParseAddr(host); err != nil {
		for _, ch := range host {
			if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || ch == '-' || ch == '.') {
				return "", errHTTPAuthority
			}
		}
	}
	port := value.Port()
	if port == "" {
		port = "443"
	}
	n, err := strconv.ParseUint(port, 10, 16)
	if err != nil || n == 0 {
		return "", errHTTPAuthority
	}
	return strings.ToLower(host) + ":" + strconv.FormatUint(n, 10), nil
}

func newNativeHTTPTransport(ctx context.Context, endpoint string) (*nativeHTTPTransport, error) {
	if ctx == nil || len(endpoint) > maxHTTPURLBytes {
		return nil, errHTTPArgument
	}
	parsed, err := url.Parse(endpoint)
	if err != nil || parsed.Path != "" && parsed.Path != "/" || parsed.RawQuery != "" || parsed.ForceQuery {
		return nil, errHTTPAuthority
	}
	authority, err := httpAuthority(parsed)
	if err != nil {
		return nil, err
	}
	return &nativeHTTPTransport{ctx: ctx, authority: authority}, nil
}

func validHTTPHeader(name string, values []string) bool {
	if name == "" || len(values) == 0 {
		return false
	}
	for _, ch := range name {
		if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || strings.ContainsRune("!#$%&'*+-.^_`|~", ch)) {
			return false
		}
	}
	for _, value := range values {
		for _, ch := range value {
			if ch > 126 || ch < 32 && ch != '\t' {
				return false
			}
		}
	}
	return true
}

// prepareNativeHTTPRequest owns and closes Body even when validation refuses.
// Blocking local reads/Close remain on the owning worker, never detached.
func (transport *nativeHTTPTransport) prepareNativeHTTPRequest(request *http.Request) (wire nativeHTTPRequest, body []byte, err error) {
	if request == nil {
		return wire, nil, errHTTPArgument
	}
	if request.Body != nil {
		defer func() {
			closeErr := request.Body.Close()
			if closeErr != nil && err == nil {
				err = errHTTPBody
			}
			if err != nil {
				zeroBytes(body)
				body = nil
			}
		}()
	}
	authority, authErr := httpAuthority(request.URL)
	if authErr != nil || authority != transport.authority {
		return wire, nil, errHTTPAuthority
	}
	if request.Host != "" && request.Host != request.URL.Host {
		// Host participates in SigV4 bytes. Do not admit an equivalent spelling
		// and then silently replace it with the URL spelling in Foundation.
		return wire, nil, errHTTPAuthority
	}
	if request.RequestURI != "" || request.URL.ForceQuery || request.Trailer != nil || len(request.TransferEncoding) != 0 {
		return wire, nil, errHTTPArgument
	}
	if request.URL.RawPath != "" && request.URL.RawPath != request.URL.EscapedPath() {
		return wire, nil, errHTTPArgument
	}
	switch request.Method {
	case http.MethodGet, http.MethodHead, http.MethodDelete:
		if request.ContentLength != 0 || request.Body != nil && request.Body != http.NoBody {
			return wire, nil, errHTTPArgument
		}
	case http.MethodPut:
	default:
		return wire, nil, errHTTPArgument
	}
	if request.ContentLength < 0 {
		return wire, nil, errHTTPArgument
	}
	if request.ContentLength > maxHTTPObjectBytes || len(request.Header) > 256 {
		return wire, nil, errHTTPLimit
	}
	if len(request.URL.Path) > maxHTTPURLBytes || len(request.URL.RawPath) > maxHTTPURLBytes || len(request.URL.RawQuery) > maxHTTPURLBytes {
		return wire, nil, errHTTPLimit
	}
	wire.URL = request.URL.String()
	if len(wire.URL) > maxHTTPURLBytes {
		return wire, nil, errHTTPLimit
	}
	if _, err := url.ParseQuery(request.URL.RawQuery); err != nil {
		return wire, nil, errHTTPArgument
	}
	wire.Method = request.Method
	wire.Headers = make(map[string][]string, len(request.Header))
	seenNames := make(map[string]bool, len(request.Header))
	headerBytes := 0
	headerValues := 0
	for name, values := range request.Header {
		headerValues += len(values)
		if len(name) > maxHTTPHeaderBytes || headerValues > 256 {
			return wire, nil, errHTTPLimit
		}
		if !validHTTPHeader(name, values) {
			return wire, nil, errHTTPArgument
		}
		lowerName := strings.ToLower(name)
		if seenNames[lowerName] {
			return wire, nil, errHTTPArgument
		}
		seenNames[lowerName] = true
		switch lowerName {
		case "cookie", "cookie2", "proxy-authorization", "proxy-connection", "trailer", "transfer-encoding":
			return wire, nil, errHTTPArgument
		case "host":
			if len(values) != 1 || values[0] != request.URL.Host {
				return wire, nil, errHTTPAuthority
			}
		case "content-length":
			if len(values) != 1 || values[0] != strconv.FormatInt(request.ContentLength, 10) {
				return wire, nil, errHTTPArgument
			}
		}
		headerBytes += len(name)
		for _, value := range values {
			if len(value) > maxHTTPHeaderBytes-headerBytes {
				return wire, nil, errHTTPLimit
			}
			headerBytes += len(value)
		}
		if headerBytes > maxHTTPHeaderBytes {
			return wire, nil, errHTTPLimit
		}
		wire.Headers[name] = append([]string(nil), values...)
	}
	if request.Body == nil || request.Body == http.NoBody {
		if request.ContentLength != 0 {
			return wire, nil, errHTTPArgument
		}
		return wire, nil, nil
	}
	body, readErr := io.ReadAll(io.LimitReader(request.Body, request.ContentLength+1))
	if readErr != nil {
		return wire, body, errHTTPBody
	}
	if int64(len(body)) != request.ContentLength {
		return wire, body, errHTTPBody
	}
	return wire, body, nil
}

func (transport *nativeHTTPTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	wire, body, err := transport.prepareNativeHTTPRequest(request)
	if err != nil {
		return nil, err
	}
	defer zeroBytes(body)
	encoded, err := json.Marshal(wire)
	if err != nil {
		return nil, errHTTPArgument
	}
	defer zeroBytes(encoded)
	if len(encoded) > maxHTTPHeaderBytes {
		return nil, errHTTPLimit
	}
	ctx, cancel := context.WithCancel(transport.ctx)
	requestCancelled := make(chan struct{})
	stopRequestCancellation := context.AfterFunc(request.Context(), func() {
		cancel()
		close(requestCancelled)
	})
	defer func() {
		if !stopRequestCancellation() {
			<-requestCancelled
		}
		cancel()
	}()
	if ctx.Err() != nil {
		return nil, context.Canceled
	}
	limit := maxHTTPObjectBytes
	if request.URL.Query().Get("list-type") == "2" {
		limit = maxHTTPListBytes
	}
	reply, err := performNativeHTTP(ctx, encoded, body, limit)
	if err != nil {
		return nil, err
	}
	if reply.Status < 100 || reply.Status > 599 || len(reply.Headers) > 256 || len(reply.Body) > limit {
		zeroBytes(reply.Body)
		return nil, errHTTPProtocol
	}
	headers := make(http.Header, len(reply.Headers))
	headerBytes := 0
	for name, values := range reply.Headers {
		if !validHTTPHeader(name, values) {
			zeroBytes(reply.Body)
			return nil, errHTTPProtocol
		}
		headerBytes += len(name)
		for _, value := range values {
			headerBytes += len(value)
		}
		if headerBytes > maxHTTPHeaderBytes {
			zeroBytes(reply.Body)
			return nil, errHTTPLimit
		}
		for _, value := range values {
			headers.Add(name, value)
		}
	}
	length := int64(len(reply.Body))
	if value := headers.Get("Content-Length"); value != "" {
		length, err = strconv.ParseInt(value, 10, 64)
		if err != nil || length < 0 || request.Method != http.MethodHead && length != int64(len(reply.Body)) {
			zeroBytes(reply.Body)
			return nil, errHTTPProtocol
		}
	}
	return &http.Response{
		StatusCode: reply.Status,
		Status:     strconv.Itoa(reply.Status) + " " + http.StatusText(reply.Status),
		Header:     headers, ContentLength: length, Request: request,
		Body: &nativeHTTPResponseBody{reader: bytes.NewReader(reply.Body), owned: reply.Body},
	}, nil
}

type nativeHTTPResponseBody struct {
	mu     sync.Mutex
	reader *bytes.Reader
	owned  []byte
}

func (body *nativeHTTPResponseBody) Read(buffer []byte) (int, error) {
	body.mu.Lock()
	defer body.mu.Unlock()
	if body.reader == nil {
		return 0, io.ErrClosedPipe
	}
	return body.reader.Read(buffer)
}

func (body *nativeHTTPResponseBody) Close() error {
	body.mu.Lock()
	defer body.mu.Unlock()
	zeroBytes(body.owned)
	body.owned = nil
	body.reader = nil
	return nil
}

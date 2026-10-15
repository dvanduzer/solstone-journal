//go:build integration && (darwin || ios)

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

// Controlled protocol and runtime interception exist only in the explicitly
// classified integration harness. Production archives omit this file.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#include <stdint.h>

static NSCondition *SBTestCondition;
static int SBTestMode;
static unsigned SBTestReached;
static unsigned SBTestReleased;
static NSData *SBTestSnapshot;
static NSDictionary *SBTestRequest;
static BOOL SBTestStoresClosed;
static unsigned SBTestActive;
static NSOperationQueue *SBTestQueue;
static id<NSURLSessionDataDelegate> SBTestDelegate;
static NSURLSession *SBTestSessionValue;
static int32_t SBTestWaitCode;
static BOOL SBTestPolicyRefused;

static IMP SBOriginalConfiguration;
static IMP SBOriginalSession;
static IMP SBOriginalComplete;
static IMP SBOriginalInvalid;
static IMP SBOriginalCancel;
static IMP SBOriginalWait;

static void SBTestEnter(void) {
    [SBTestCondition lock];
    SBTestActive++;
    [SBTestCondition unlock];
}

static void SBTestExit(unsigned milestone) {
    [SBTestCondition lock];
    SBTestReached |= milestone;
    SBTestActive--;
    [SBTestCondition broadcast];
    [SBTestCondition unlock];
}

static void SBTestPause(unsigned stage) {
    [SBTestCondition lock];
    SBTestReached |= stage;
    [SBTestCondition broadcast];
    while (!(SBTestReleased & stage)) [SBTestCondition wait];
    [SBTestCondition unlock];
}

@interface SBHTTPFixtureProtocol : NSURLProtocol
@end

@implementation SBHTTPFixtureProtocol
+ (BOOL)canInitWithRequest:(NSURLRequest *)request {
    return [request.URL.host isEqualToString:@"storage.example"];
}
+ (NSURLRequest *)canonicalRequestForRequest:(NSURLRequest *)request { return request; }
- (void)startLoading {
    NSData *body = self.request.HTTPBody ?: [NSData data];
    if (self.request.HTTPBodyStream) {
        NSInputStream *stream = self.request.HTTPBodyStream;
        NSMutableData *copied = [NSMutableData data];
        uint8_t buffer[1024];
        [stream open];
        NSInteger count;
        while ((count = [stream read:buffer maxLength:sizeof(buffer)]) > 0 && copied.length <= 65536) {
            [copied appendBytes:buffer length:(NSUInteger)count];
        }
        [stream close];
        body = copied;
    }
    [SBTestCondition lock];
    [SBTestRequest release];
    SBTestRequest = [@{@"url":self.request.URL.absoluteString ?: @"", @"method":self.request.HTTPMethod ?: @"",
        @"token":[self.request valueForHTTPHeaderField:@"X-Amz-Security-Token"] ?: @"",
        @"authorization":[self.request valueForHTTPHeaderField:@"Authorization"] ?: @"",
        @"body_base64":[body base64EncodedStringWithOptions:0],
        @"body_bytes":@(body.length), @"stores_closed":@(SBTestStoresClosed)} retain];
    SBTestReached |= 8;
    [SBTestCondition broadcast];
    int mode = SBTestMode;
    [SBTestCondition unlock];
    if (mode == 3) return; // Wait for actual native task cancellation.
    // Invoke the actual policy delegates explicitly. These controlled calls are
    // policy tests, not a claim that NSURLProtocol performed a wire redirect or
    // authentication handshake.
    if (mode == 11) {
        NSURLRequest *redirect = [NSURLRequest requestWithURL:[NSURL URLWithString:@"https://other.example/secret"]];
        [SBTestDelegate URLSession:SBTestSessionValue task:nil willPerformHTTPRedirection:nil newRequest:redirect
            completionHandler:^(NSURLRequest *request) {
                [SBTestCondition lock]; SBTestPolicyRefused = request == nil; [SBTestCondition unlock];
            }];
    } else if (mode == 12) {
        NSURLProtectionSpace *space = [[[NSURLProtectionSpace alloc] initWithHost:@"storage.example" port:443
            protocol:@"https" realm:@"fixture" authenticationMethod:NSURLAuthenticationMethodHTTPBasic] autorelease];
        NSURLAuthenticationChallenge *challenge = [[[NSURLAuthenticationChallenge alloc] initWithProtectionSpace:space
            proposedCredential:nil previousFailureCount:0 failureResponse:nil error:nil sender:nil] autorelease];
        [SBTestDelegate URLSession:SBTestSessionValue task:nil didReceiveChallenge:challenge
            completionHandler:^(NSURLSessionAuthChallengeDisposition disposition, NSURLCredential *credential) {
                [SBTestCondition lock];
                SBTestPolicyRefused = disposition == NSURLSessionAuthChallengeCancelAuthenticationChallenge && credential == nil;
                [SBTestCondition unlock];
            }];
    }
    NSInteger status = 200;
    NSDictionary *headers = @{@"Content-Type":@"application/octet-stream", @"Content-Length":@"7"};
    NSData *reply = [@"fixture" dataUsingEncoding:NSUTF8StringEncoding];
    if (mode == 4) {
        headers = @{@"Content-Type":@"application/xml"};
        reply = [NSMutableData dataWithLength:1024 * 1024 + 1];
    } else if (mode == 5) {
        headers = @{@"Content-Type":@"application/octet-stream", @"Content-Length":@"100000000"};
    } else if (mode == 6) {
        headers = @{@"Content-Type":@"multipart/x-mixed-replace; boundary=fixture"};
    } else if (mode == 7) {
        headers = @{@"Content-Type":@"application/octet-stream", @"Content-Length":@"1"};
    } else if (mode == 9) {
        status = 300;
        headers = @{@"Content-Type":@"application/xml"};
        reply = [NSMutableData dataWithLength:64 * 1024 + 1];
    } else if (mode == 10) {
        headers = @{@"Content-Type":@"application/octet-stream", @"Content-Length":@"100000000"};
        reply = [NSData data];
    } else if (mode == 13) {
        headers = @{@"Content-Encoding":@"gzip"};
    } else if (mode == 14) {
        headers = @{@"X-Oversized":[@"x" stringByPaddingToLength:32 * 1024 + 1 withString:@"x" startingAtIndex:0]};
    } else if (mode == 16) {
        headers = @{@"Content-Length":@"0"};
        reply = [NSData data];
    }
    NSHTTPURLResponse *response = [[[NSHTTPURLResponse alloc] initWithURL:self.request.URL statusCode:status
        HTTPVersion:@"HTTP/1.1" headerFields:headers] autorelease];
    [self.client URLProtocol:self didReceiveResponse:response cacheStoragePolicy:NSURLCacheStorageNotAllowed];
    if (mode == 15) [self.client URLProtocol:self didReceiveResponse:response cacheStoragePolicy:NSURLCacheStorageNotAllowed];
    if (reply.length) [self.client URLProtocol:self didLoadData:reply];
    [self.client URLProtocolDidFinishLoading:self];
}
- (void)stopLoading {}
@end

static id SBTestConfiguration(id self, SEL selector) {
    SBTestEnter();
    NSURLSessionConfiguration *configuration = ((id (*)(id, SEL))SBOriginalConfiguration)(self, selector);
    configuration.protocolClasses = @[[SBHTTPFixtureProtocol class]];
    SBTestExit(0);
    return configuration;
}

static id SBTestSession(id self, SEL selector, NSURLSessionConfiguration *configuration, id delegate, NSOperationQueue *queue) {
    SBTestEnter();
    [SBTestCondition lock];
    [SBTestQueue release];
    SBTestQueue = [queue retain];
    SBTestDelegate = [delegate retain];
    SBTestStoresClosed = configuration.URLCredentialStorage == nil && configuration.HTTPCookieStorage == nil &&
        configuration.URLCache == nil && !configuration.HTTPShouldSetCookies &&
        configuration.connectionProxyDictionary != nil && configuration.connectionProxyDictionary.count == 0 &&
        queue.maxConcurrentOperationCount == 1;
    [SBTestCondition unlock];
    id result = ((id (*)(id, SEL, id, id, id))SBOriginalSession)(self, selector, configuration, delegate, queue);
    SBTestSessionValue = [result retain];
    SBTestExit(0);
    return result;
}

static void SBTestComplete(id self, SEL selector, NSURLSession *session, NSURLSessionTask *task, NSError *error) {
    SBTestEnter();
    ((void (*)(id, SEL, id, id, id))SBOriginalComplete)(self, selector, session, task, error);
    if (SBTestMode == 1) SBTestPause(1);
    SBTestExit(0);
}

static void SBTestInvalid(id self, SEL selector, NSURLSession *session, NSError *error) {
    SBTestEnter();
    ((void (*)(id, SEL, id, id))SBOriginalInvalid)(self, selector, session, error);
    if (SBTestMode == 2) SBTestPause(2);
    SBTestExit(32);
}

static void SBTestCancel(id self, SEL selector) {
    SBTestEnter();
    ((void (*)(id, SEL))SBOriginalCancel)(self, selector);
    if (SBTestMode == 3) SBTestPause(4);
    SBTestExit(0);
}

static int32_t SBTestWait(id self, SEL selector) {
    SBTestEnter();
    int32_t result = ((int32_t (*)(id, SEL))SBOriginalWait)(self, selector);
    [SBTestCondition lock];
    SBTestWaitCode = result;
    [SBTestCondition unlock];
    SBTestExit(16);
    return result;
}

int32_t sb_http_test_install(int32_t mode) {
    @autoreleasepool {
        if (SBTestCondition || mode < 1 || mode > 18) return 1;
        Class call = objc_getClass("SBHTTPCall");
        if (!call) return 1;
        SBTestCondition = [[NSCondition alloc] init];
        SBTestMode = mode;
        SBTestReached = SBTestReleased = 0;
        SBTestStoresClosed = NO;
        SBTestActive = 0;
        SBTestWaitCode = -1;
        SBTestPolicyRefused = NO;
        SBOriginalConfiguration = method_setImplementation(class_getClassMethod([NSURLSessionConfiguration class],
            @selector(ephemeralSessionConfiguration)), (IMP)SBTestConfiguration);
        SBOriginalSession = method_setImplementation(class_getClassMethod([NSURLSession class],
            @selector(sessionWithConfiguration:delegate:delegateQueue:)), (IMP)SBTestSession);
        SBOriginalComplete = method_setImplementation(class_getInstanceMethod(call,
            @selector(URLSession:task:didCompleteWithError:)), (IMP)SBTestComplete);
        SBOriginalInvalid = method_setImplementation(class_getInstanceMethod(call,
            @selector(URLSession:didBecomeInvalidWithError:)), (IMP)SBTestInvalid);
        SBOriginalCancel = method_setImplementation(class_getInstanceMethod(call, @selector(cancel)), (IMP)SBTestCancel);
        SBOriginalWait = method_setImplementation(class_getInstanceMethod(call, @selector(waitAndJoin)), (IMP)SBTestWait);
        return 0;
    }
}

int32_t sb_http_test_wait(uint32_t stage) {
    @autoreleasepool {
        if (!SBTestCondition) return 1;
        [SBTestCondition lock];
        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:10];
        while (!(SBTestReached & stage)) {
            if (![SBTestCondition waitUntilDate:deadline]) { [SBTestCondition unlock]; return 1; }
        }
        [SBTestCondition unlock];
        return 0;
    }
}

void sb_http_test_release(uint32_t stage) {
    [SBTestCondition lock];
    SBTestReleased |= stage;
    [SBTestCondition broadcast];
    [SBTestCondition unlock];
}

int32_t sb_http_test_drain(void) {
    @autoreleasepool {
        [SBTestCondition lock];
        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:10];
        // Independent of the production RoundTrip return: the final session
        // callback tail and every active wrapper must actually have left.
        while (!(SBTestReached & 32) || SBTestActive != 0) {
            if (![SBTestCondition waitUntilDate:deadline]) { [SBTestCondition unlock]; return 1; }
        }
        NSOperationQueue *queue = [SBTestQueue retain];
        [SBTestCondition unlock];
        // Join the captured real queue independently of production waitAndJoin.
        [queue waitUntilAllOperationsAreFinished];
        [queue release];
        return 0;
    }
}

const void *sb_http_test_snapshot(uint32_t *length) {
    @autoreleasepool {
        [SBTestCondition lock];
        [SBTestSnapshot release];
        NSMutableDictionary *snapshot = [NSMutableDictionary dictionaryWithDictionary:SBTestRequest ?: @{}];
        snapshot[@"native_code"] = @(SBTestWaitCode);
        snapshot[@"policy_refused"] = @(SBTestPolicyRefused);
        SBTestSnapshot = [[NSJSONSerialization dataWithJSONObject:snapshot options:0 error:NULL] retain];
        *length = (uint32_t)SBTestSnapshot.length;
        const void *result = SBTestSnapshot.bytes;
        [SBTestCondition unlock];
        return result;
    }
}

void sb_http_test_uninstall(void) {
    @autoreleasepool {
        Class call = objc_getClass("SBHTTPCall");
        method_setImplementation(class_getClassMethod([NSURLSessionConfiguration class], @selector(ephemeralSessionConfiguration)), SBOriginalConfiguration);
        method_setImplementation(class_getClassMethod([NSURLSession class], @selector(sessionWithConfiguration:delegate:delegateQueue:)), SBOriginalSession);
        method_setImplementation(class_getInstanceMethod(call, @selector(URLSession:task:didCompleteWithError:)), SBOriginalComplete);
        method_setImplementation(class_getInstanceMethod(call, @selector(URLSession:didBecomeInvalidWithError:)), SBOriginalInvalid);
        method_setImplementation(class_getInstanceMethod(call, @selector(cancel)), SBOriginalCancel);
        method_setImplementation(class_getInstanceMethod(call, @selector(waitAndJoin)), SBOriginalWait);
        [SBTestRequest release]; SBTestRequest = nil;
        [SBTestSnapshot release]; SBTestSnapshot = nil;
        [SBTestQueue release]; SBTestQueue = nil;
        [SBTestDelegate release]; SBTestDelegate = nil;
        [SBTestSessionValue release]; SBTestSessionValue = nil;
        [SBTestCondition release]; SBTestCondition = nil;
    }
}

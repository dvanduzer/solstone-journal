// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Foundation
import SwiftParser
import SwiftSyntax

struct Literal: Encodable {
    let start: Int
    let end: Int
    let raw: String
    let kind: String
    let context: String
}
final class Discovery: SyntaxVisitor {
    var literals: [Literal] = []
    let bytes: [UInt8]
    init(source: String) { bytes = Array(source.utf8); super.init(viewMode: .sourceAccurate) }
    override func visit(_ node: StringLiteralExprSyntax) -> SyntaxVisitorContinueKind {
        let start = node.positionAfterSkippingLeadingTrivia.utf8Offset
        let end = node.endPositionBeforeTrailingTrivia.utf8Offset
        let ancestors = sequence(first: node.parent, next: { $0?.parent }).compactMap { $0 }
        let macro = ancestors.first { $0.is(MacroExpansionExprSyntax.self) || $0.is(MacroExpansionDeclSyntax.self) }
        literals.append(Literal(start: start, end: end, raw: String(decoding: bytes[start..<end], as: UTF8.self), kind: "string", context: macro == nil ? "expression" : "macro"))
        // Interpolations remain whole editable expressions. Nested string tokens are inside this span.
        return .skipChildren
    }
}
do {
    guard CommandLine.arguments.count == 2 else { throw NSError(domain: "usage: swift-discover SOURCE.swift", code: 1) }
    let source = try String(contentsOfFile: CommandLine.arguments[1], encoding: .utf8)
    let syntax = Parser.parse(source: source)
    guard !syntax.hasError else { throw NSError(domain: "Swift source contains parser errors", code: 1) }
    let discovery = Discovery(source: source)
    discovery.walk(syntax)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    FileHandle.standardOutput.write(try encoder.encode(discovery.literals.sorted { $0.start < $1.start }))
    FileHandle.standardOutput.write(Data([10]))
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(1)
}

// Inspect only a disposable test keychain; no production keychain defaults.
import Foundation
import Darwin
import Security
func require(_ condition: Bool, _ message: String) {
    if !condition { fputs(message + "\n", stderr); exit(1) }
}
require(CommandLine.arguments.count == 3, "Pass the disposable keychain and compiled helper paths")
var chain: SecKeychain?
require(SecKeychainOpen(CommandLine.arguments[1], &chain) == errSecSuccess, "Open test keychain")
let label = "li.rajeshgo.sm.device.qa-device"
var result: CFTypeRef?
let query: [String: Any] = [kSecClass as String: kSecClassKey,
    kSecAttrKeyClass as String: kSecAttrKeyClassPrivate,
    kSecAttrApplicationTag as String: Data(label.utf8),
    kSecMatchSearchList as String: [chain!], kSecReturnRef as String: true]
require(SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess, "Find test private key")
let key = result as! SecKey
var access: SecAccess?
require(SecKeychainItemCopyAccess(unsafeBitCast(key, to: SecKeychainItem.self), &access) == errSecSuccess, "Read key access")
var entries: CFArray?
require(SecAccessCopyACLList(access!, &entries) == errSecSuccess, "Read key access entries")
var checked = false
for entry in entries as! [SecACL] {
    let authorizations = SecACLCopyAuthorizations(entry) as! [String]
    guard authorizations.contains(kSecACLAuthorizationSign as String) else { continue }
    var applications: CFArray?
    var description: CFString?
    var prompt = SecKeychainPromptSelector(rawValue: 0)
    require(SecACLCopyContents(entry, &applications, &description, &prompt) == errSecSuccess, "Read signing permissions")
    require(description as String? == label, "Signing prompt must identify the device, not <key>")
    guard let trusted = applications as? [SecTrustedApplication] else { require(false, "Signing must not trust all applications"); exit(1) }
    require(trusted.count == 2, "Only helper and Chrome should be trusted")
    var chrome: SecTrustedApplication?
    require(SecTrustedApplicationCreateFromPath("/Applications/Google Chrome.app", &chrome) == errSecSuccess, "Identify installed Chrome")
    var expected: CFData?
    require(SecTrustedApplicationCopyData(chrome!, &expected) == errSecSuccess, "Read Chrome identity")
    require(trusted.contains {
        var actual: CFData?
        return SecTrustedApplicationCopyData($0, &actual) == errSecSuccess && actual == expected
    }, "Chrome must be explicitly trusted for signing")
    var helper: SecTrustedApplication?
    // SecTrustedApplicationCreateFromPath(nil) records the executable's real
    // path; macOS TMPDIR commonly uses /var, a symlink to /private/var.
    guard let resolvedPath = realpath(CommandLine.arguments[2], nil) else {
        require(false, "Resolve compiled helper path"); exit(1)
    }
    let helperPath = String(cString: resolvedPath)
    free(resolvedPath)
    require(SecTrustedApplicationCreateFromPath(helperPath, &helper) == errSecSuccess, "Identify compiled helper")
    var helperData: CFData?
    require(SecTrustedApplicationCopyData(helper!, &helperData) == errSecSuccess, "Read compiled helper identity")
    require(trusted.contains {
        var actual: CFData?
        return SecTrustedApplicationCopyData($0, &actual) == errSecSuccess && actual == helperData
    }, "The second trusted app must be the compiled helper, not Swift")
    checked = true
}
require(checked, "No signing permission entry found")
print("Device signing prompt and Chrome permissions passed.")

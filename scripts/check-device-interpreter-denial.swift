// This intentionally runs through the general Swift interpreter. Never use a
// production keychain: the caller must provide a disposable test keychain.
import Foundation
import Security
guard CommandLine.arguments.count == 2 else { exit(1) }
SecKeychainSetUserInteractionAllowed(false)
var chain: SecKeychain?, result: CFTypeRef?
guard SecKeychainOpen(CommandLine.arguments[1], &chain) == errSecSuccess else { exit(2) }
let query: [String: Any] = [kSecClass as String: kSecClassKey,
    kSecAttrKeyClass as String: kSecAttrKeyClassPrivate,
    kSecAttrApplicationTag as String: Data("li.rajeshgo.sm.device.qa-device".utf8),
    kSecMatchSearchList as String: [chain!], kSecReturnRef as String: true]
guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess else { exit(3) }
var error: Unmanaged<CFError>?
let signature = SecKeyCreateSignature(result as! SecKey, .ecdsaSignatureMessageX962SHA256,
    Data("unrelated Swift script must not sign".utf8) as CFData, &error)
guard signature == nil, error != nil else {
    fputs("General Swift interpreter unexpectedly signed with the device key\n", stderr)
    exit(4)
}
print("Unrelated Swift interpreter signing was denied without a prompt.")

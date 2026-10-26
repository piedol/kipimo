// Generates btm-v16-synthetic.btm: a Background Task Management database with made-up
// records, archived by the real NSKeyedArchiver with the same class and field names macOS 26
// uses (Storage / ItemRecord / BTMUserSettings). Run: swift make_btm_fixture.swift <out.btm>
import Foundation

@objc(ItemRecord) class ItemRecord: NSObject, NSCoding {
    var f: [String: Any?]
    init(_ f: [String: Any?]) { self.f = f }
    required init?(coder: NSCoder) { return nil }
    func encode(with c: NSCoder) {
        let objectKeys = ["associatedBundleIdentifiers", "bookmark", "bundleIdentifier", "container",
                          "designatedRequirement", "developerName", "executablePath", "identifier", "items",
                          "lightweightRequirement", "name", "programArguments", "sha256", "teamIdentifier", "url"]
        for k in objectKeys { c.encode(f[k] ?? nil, forKey: k) }
        c.encode((f["disposition"] as? Int) ?? 0, forKey: "disposition")
        c.encode((f["type"] as? Int) ?? 0, forKey: "type")
        c.encode((f["flags"] as? Int) ?? 0, forKey: "flags")
        c.encode((f["generation"] as? Int) ?? 1, forKey: "generation")
        c.encode(0.0, forKey: "modificationDate")
        c.encode(0.0, forKey: "executableModificationDate")
        c.encode(UUID(), forKey: "uuid")
    }
}

@objc(BTMUserSettings) class BTMUserSettings: NSObject, NSCoding {
    override init() {}
    required init?(coder: NSCoder) { return nil }
    func encode(with c: NSCoder) {
        c.encode(1, forKey: "generation")
        c.encode(true, forKey: "serviceManagementLoginItemsMigrated")
        c.encode(NSDictionary(), forKey: "enablementStatus")
    }
}

@objc(Storage) class Storage: NSObject, NSCoding {
    var items: [String: [ItemRecord]]
    init(_ items: [String: [ItemRecord]]) { self.items = items }
    required init?(coder: NSCoder) { return nil }
    func encode(with c: NSCoder) {
        c.encode(items as NSDictionary, forKey: "itemsByUserIdentifier")
        c.encode(NSDictionary(), forKey: "mdmPaloadsByIdentifier")
        c.encode(["11111111-2222-3333-4444-555555555555": BTMUserSettings()] as NSDictionary,
                 forKey: "userSettingsByUserIdentifier")
    }
}

let user = "11111111-2222-3333-4444-555555555555"
let system = "FFFFEEEE-DDDD-CCCC-BBBB-AAAAFFFFFFFE"
let userRecords: [ItemRecord] = [
    // An app that opens at login (SMAppService.mainApp).
    ItemRecord(["identifier": "2.com.example.notes", "name": "Example Notes", "developerName": "Example Corp",
                "teamIdentifier": "EXAMPLE123", "bundleIdentifier": "com.example.notes", "type": 0x2,
                "disposition": 0xb, "url": URL(string: "file:///Applications/Example%20Notes.app/")!]),
    // An app that is only a parent of other items (disabled itself).
    ItemRecord(["identifier": "2.com.example.sync", "name": "Example Sync", "teamIdentifier": "EXAMPLE123",
                "bundleIdentifier": "com.example.sync", "type": 0x2, "disposition": 0xa,
                "url": URL(string: "file:///Applications/Example%20Sync.app/")!,
                "items": ["4.com.example.sync.launcher"]]),
    // An SMLoginItemSetEnabled helper, URL relative to its container app; turned off.
    ItemRecord(["identifier": "4.com.example.sync.launcher", "name": "Sync Launcher", "teamIdentifier": "EXAMPLE123",
                "bundleIdentifier": "com.example.sync.launcher", "type": 0x4, "disposition": 0xa,
                "container": "2.com.example.sync",
                "url": URL(string: "Contents/Library/LoginItems/Sync%20Launcher.app", relativeTo: URL(string: "file:///")!)!]),
    // A developer group record.
    ItemRecord(["identifier": "Example Corp", "name": "Example Corp", "developerName": "Example Corp",
                "type": 0x20, "disposition": 0x2]),
    // A legacy user agent (plist in ~/Library/LaunchAgents).
    ItemRecord(["identifier": "8.com.example.updater", "name": "Example Updater", "developerName": "Example Corp",
                "teamIdentifier": "EXAMPLE123", "type": 0x10008, "disposition": 0xb, "flags": 1,
                "container": "Example Corp", "associatedBundleIdentifiers": ["com.example.notes"],
                "executablePath": "/Users/tester/Library/Application Support/Example/Updater",
                "programArguments": ["/Users/tester/Library/Application Support/Example/Updater", "--check"],
                "url": URL(string: "file:///Users/tester/Library/LaunchAgents/com.example.updater.plist")!]),
    // A non-legacy SMAppService agent inside an app bundle.
    ItemRecord(["identifier": "8.com.example.notes.agent", "name": "Notes Agent", "teamIdentifier": "EXAMPLE123",
                "type": 0x8, "disposition": 0x3, "container": "2.com.example.notes",
                "executablePath": "/Applications/Example Notes.app/Contents/MacOS/NotesAgent",
                "url": URL(string: "Contents/Library/LaunchAgents/com.example.notes.agent.plist", relativeTo: URL(string: "file:///")!)!]),
    // A Spotlight importer (not a startup item).
    ItemRecord(["identifier": "64.com.example.importer", "name": "Example.mdimporter", "type": 0x40,
                "disposition": 0xb, "container": "2.com.example.notes",
                "url": URL(string: "Contents/Library/Spotlight/Example.mdimporter", relativeTo: URL(string: "file:///")!)!]),
]
let systemRecords: [ItemRecord] = [
    ItemRecord(["identifier": "16.com.example.helperd", "name": "com.example.helperd", "developerName": "Example Corp",
                "teamIdentifier": "EXAMPLE123", "type": 0x10010, "disposition": 0xb, "flags": 1,
                "container": "2.com.example.sync",
                "executablePath": "/Library/PrivilegedHelperTools/com.example.helperd",
                "url": URL(string: "file:///Library/LaunchDaemons/com.example.helperd.plist")!]),
]
let archiver = NSKeyedArchiver(requiringSecureCoding: false)
archiver.encode(Storage([user: userRecords, system: systemRecords]), forKey: "store")
archiver.encode(16, forKey: "version")
archiver.finishEncoding()
try! archiver.encodedData.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))

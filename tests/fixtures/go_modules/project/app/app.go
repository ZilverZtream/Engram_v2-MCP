package app
import storage "example.com/engram-corpus/primary"
import backup "example.com/engram-corpus/alternate"
func RunPrimary() int { return storage.Save() }
func RunAlternate() int { return backup.Save() }
func RunLocal() int { return helper() }

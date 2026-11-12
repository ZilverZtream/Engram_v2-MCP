from primary import save as persist
import alternate as backup
def run_primary():
    return persist()
def run_alternate():
    return backup.save()
def run_local():
    return helper()
def helper():
    return 3

# Deprecated

## This project has moved to https://github.com/AOSC-Dev/repokit/tree/master/repo-notifier.

Deploying
---------

### Create A Bot

Contact @BotFather to create a new bot, a bot token will be provided by BotFather.

### Find the ZeroMQ Interface Address

The address is placed in the configuration file of p-vector (or other compatible software)
The address looks like `tcp://repo.aosc.io:xxxxx`.

### Compile the Bot

You will need the following packages:

```
rustc libssl-dev libzmq3-dev pkg-config build-essential
```

Then `cargo build --release` and `install -Dm755 target/release/repository-notifier /usr/local/bin/repository-notifier`.

### Install systemd Services

```bash
install -Dm644 assets/repo-notifier.service /etc/systemd/system/repo-notifier.service
install -Dm644 assets/repo-notifier.conf /etc/repo-notifier.conf
```

### Edit Configurations

Edit `/etc/repo-notifier.conf` and put the values you found in previous steps into the configuration file.

### Launch the Bot

Enter `sudo systemctl start repo-notifier.service`.

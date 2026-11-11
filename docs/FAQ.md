# Frequently asked questions

Quick answers about the Agora desktop app, with links to more detail. If something isn't working, see [Troubleshooting Agora](./DESKTOP_TROUBLESHOOTING.md).

## The basics

### What is Agora?

Agora is a Minecraft launcher with a catalog of mods, packs, and more that the community curates. It keeps each modded setup separate, checks for problems before you launch, and helps you recover when something breaks.

### Is it free?

Yes. Agora is free, has no ads, and is open source. It's funded by donations.

### Which systems does it run on?

Windows is tested and signed. macOS and Linux builds are available, but they aren't officially tested yet and may not work properly.

### I play Minecraft on my console/phone/through the Microsoft store, how do I get Agora to work with it?

Agora is designed for Minecraft Java Edition, which is available on PC only. The console, phone, and Microsoft Store versions of Minecraft are freqently known as Minecraft Bedrock and do not support the same mods as Java Edition, hence why Agora only supports Java Edition.

### Do I still need to own Minecraft?

Yes. Agora starts the game, but it doesn't give you a copy of it.

### Which accounts do I need?

Only the ones for what you want to do:

- **Microsoft:** only if Agora starts the game itself (direct launch). By default the official launcher starts the game, and you sign in there.
- **GitHub:** only for voting and reading community reviews. It can also help you avoid rate limits if you do a lot of searching and downloading.

You can download mods and play without a GitHub account.

### What's an instance?

An instance is a self-contained Minecraft setup with its own version, mod loader, and mods. You can have as many as you like, and a problem in one doesn't affect the others. See [Creating and managing instances](https://agoramc.com/docs/guides/instances).

### What's the difference between delegated and direct launch?

With delegated launch, the default, Agora gets everything ready and the official launcher starts the game. With direct launch, Agora starts the game itself and shows you its log, but you need to sign in with Microsoft in Agora to play online. You can switch in Settings, under Launching. See [Launching and health checks](https://agoramc.com/docs/guides/launching).

## Mods and the catalog

### What does "Curated" mean?

A Curated project is listed in Agora's own catalog, and people have looked over its listing. It doesn't promise that every version works with your other mods, or that the project will never have a problem. See [Voting, reviews, and community governance](https://agoramc.com/docs/guides/governance).

### Can I use mods that aren't in the catalog?

Yes. Turn on Modrinth under Services to search it alongside the catalog. Results without the Curated badge may be perfectly fine, but they haven't been through Agora's review.

### How do I suggest a mod for the catalog?

Fill in the [mod submission form](https://github.com/agora-mc/Agora-Mod-Manager/issues/new?template=mod-submission.yml) on GitHub.

### Can I bring my instances from another launcher?

Yes. On My Instances, select **Import Instances** to find your Prism Launcher, CurseForge, and Modrinth App instances. Agora copies them and leaves the originals alone. Your accounts and passwords aren't imported.

### How much memory should I give Minecraft?

Enough for the pack, while leaving a few gigabytes for everything else on your computer. More memory doesn't always mean a faster game. See [Java, memory, and performance](https://agoramc.com/docs/guides/java-performance).

### Do I need to install Java?

No, not with Java set to Automatic in Settings. Agora uses a suitable Java on your computer or downloads one just for itself.

## Your data

### Does Agora track me?

No. Agora doesn't collect or send usage data about you. Features you use still need to connect to the internet, such as downloading mods, signing in, and checking for updates. Settings > Privacy lists every connection and lets you switch each one off. See [Privacy and offline use](https://agoramc.com/docs/guides/privacy-offline).

### Where are my files?

For one instance, open it and choose **Open in Folder**. For everything else, go to **Settings > Software Updates > Open application data folder**.

### Are my worlds backed up?

Not automatically. Snapshots you make yourself can include your worlds, but the automatic ones Agora takes before each launch leave them out. Snapshots also stay on your computer, so we recommend keeping your own copy of any world you can't replace. See [Snapshots and loadout profiles](https://agoramc.com/docs/guides/snapshots-loadouts).

### Can I run Agora from a USB stick?

Yes, on Windows. The portable download keeps everything in its own folder. Your sign-ins are stored in that folder too, so anyone who can open it can use them. See [Install Agora](https://agoramc.com/docs/install).

## Getting more from Agora

### How do I update Agora?

Open Settings, go to **Software Updates**, and select **Check for Updates**. Agora can download the new version and restart for you.

### Can I play with a controller?

Yes. Press a button on a connected controller and Agora adapts to it. Minecraft needs a mod called Controlify for controller input, and Agora offers to install it when you launch. See [Playing with a controller](https://agoramc.com/docs/guides/controller-handheld).

### Can I play offline?

Yes, once an instance has everything it needs. Get ready while you are online, because each instance needs its own files. See [Privacy and offline use](https://agoramc.com/docs/guides/privacy-offline).

### Can an AI assistant help me fix my game?

Yes. You can paste a crash report into one, or let an assistant app work with Agora directly and approve each change. See [Get help from an AI assistant](https://agoramc.com/docs/guides/ai-assistant).

### Where can I ask a question that isn't here?

Ask on the [Agora Discord](https://discord.gg/56tpsa2sTZ).

# Troubleshooting Agora

Find what you're seeing below and try the steps in order. If you get stuck, the getting help section at the bottom of this page explains where to look for assistance.

## Before you change anything

We recommend doing these first, so you can always get back to where you started:

1. Close the game.
2. For a Minecraft instance, open the instance and take a snapshot on the **Snapshots** tab. Instances of other games don't have snapshots yet, so for those use **Clone** on the instance card first, or back up the files you are about to change yourself.
3. Back up any world or save you can't replace somewhere outside Agora.
4. Change one thing at a time, and launch in between. That way you know what fixed it.

## Game problems

### Minecraft crashes

1. Open Crash Doctor. Select **Troubleshoot** on the instance card, or **Diagnose a crash** in the instance editor. A recent crash can also show up as an alert on Home.
2. Look at **Mods that might be causing this**. The top one is the best place to start, but it isn't guaranteed to be the cause.
3. Select **Try without** on that mod, then play until you would normally crash.
4. Answer **Yes, it worked** or **No, still crashing**. Crash Doctor uses your answer to move on to the next suspect.

If no mod stands out, use **Find the mod that breaks it**. Agora turns off half of your mods each round, you launch and report what happened, and the list of possible culprits keeps shrinking until one is left.

To stop testing and get your mods back, choose **Undo and close**.

If a suspect says curators are reviewing it, that may have nothing to do with your crash. It's still worth testing.

The [crash guide](https://agoramc.com/docs/guides/crash-recovery) has more detail.

### I just want to play again

Go back to the last setup that launched fine. Find **Last setup that launched fine** on Home and select **Restore**, or restore it from the instance's **Snapshots** tab. Agora saves this setup after you've played for at least 60 seconds without a crash. It covers your mods and settings, not your worlds.

### Agora won't let me launch

Before launching, Agora checks the instance for problems. A blocker will stop the game from working, so it's best to fix it first. A warning is something to be aware of.

Read which mod the message is about, then start with missing mods or the wrong Java, since those are the most common causes. **Launch Anyway** is there if you want to try regardless, and we suggest taking a snapshot first.

See [Launching and health checks](https://agoramc.com/docs/guides/launching) for more.

### A mod needs a different loader version

The health screen lists the affected mods and suggests a loader version that suits them all.

- Select **Switch and launch** to change to it and start the game.
- Select **Choose compatible version** to pick one yourself.

If no single version suits every mod, Agora can't switch for you. You may need to update or remove one of the mods instead.

### Java is missing or the wrong version

Set Java to **Automatic** in Settings, under Java. Agora then uses a suitable Java on your computer or downloads one just for itself. If you picked a specific Java for one instance on its **Java & Args** tab, clear it so Agora can choose again.

### The game runs out of memory or feels slow

Change the memory on the instance's **Java & Args** tab. We suggest giving Minecraft enough for the pack while leaving a few gigabytes for everything else on your computer. Extra memory won't always fix poor perfromance, poor performance can be best fixed by removing demanding mods or adding in optimization mods like Sodium. See [Java, memory, and performance](https://agoramc.com/docs/guides/java-performance).

## Launching and accounts

### The official launcher doesn't open

This applies if the official launcher starts your game, which is the default. Open Settings and go to **Launcher Path**. Try **Auto-detect**, or browse to where the launcher is installed, then test the path. If you would rather not use the official launcher, you can switch to direct launch under **Launch Mode** in the same place.

### Direct launch says it can't sign in

Check your Microsoft account in Settings, under Accounts, and sign in again if needed. Direct launch needs this to play online. If the official launcher starts your game, you sign in there instead and don't need a Microsoft sign-in in Agora.

### I can't vote

The vote arrows are grayed out until you sign in to GitHub in Settings, under Accounts. Some projects don't have voting at all. [Voting and reviews](https://agoramc.com/docs/guides/governance) explains why.

### Playing offline doesn't work

Each instance needs its own files, so get ready while you are still online:

1. Update the catalog.
2. Launch the instance once so its game and loader files download.
3. Make sure its Java is installed.
4. Turn on **Lockdown Mode** under Settings > Privacy and try launching, while you can still go back online to fix anything missing.

You can't sign in or download anything missing while offline. See [Privacy and offline use](https://agoramc.com/docs/guides/privacy-offline).

## Browsing and installing

### Browse is empty or the catalog is missing

Agora needs the internet to download its catalog the first time. Connect, then choose **Download Catalog** or **Retry**. Your installed instances keep working in the meantime.

If you are online and it still fails, check that **Lockdown Mode** is off under Settings > Privacy. Privacy is only visible with Advanced mode turned on in Settings.

If Agora says it can't verify the catalog, please don't try to get around it. Ask on the [Agora Discord](https://discord.gg/56tpsa2sTZ) instead, because the check is there to confirm the catalog really came from Agora.

### Modrinth mods aren't showing up

Modrinth has to be turned on under Services, and allowed under Privacy. Both the Modrinth API and its download server need to be allowed. If Lockdown Mode is on, everything is blocked whatever the other switches say.

The Privacy page also shows whether Agora's last attempt to reach each server worked, which tells you whether the problem is your settings or your internet.

### An install asks me to decide something

Agora stops and asks when mods conflict, or when something isn't available for your Minecraft version or loader. Read the review screen before continuing, and take a snapshot first if the instance matters to you. See [Installing and updating](https://agoramc.com/docs/guides/install-update).

### Windows says "Access denied" when I change a mod file myself

Agora keeps one copy of each mod and shares it between the instances that use it, so ten instances
with the same mod use the disk space of one. The files in a Minecraft instance's `mods`,
`resourcepacks`, `shaderpacks` and `datapacks` folders are links to that shared copy, and the copy is
protected so a change made through one instance can't quietly break another.

You may see this when you work in those folders with File Explorer or another tool:

- replacing a mod with a newer file of the same name says "Access denied" or "You need permission";
- a tool that edits mod files in place fails to save;
- File Explorer reports each instance at its full size, even though the mods are only stored once.

What to do:

- Add, update and remove content in Agora (the Instance Editor or Browse) where you can.
- Deleting or renaming a mod file in File Explorer works. To replace one by hand, delete the old file
  first, then add the new one.
- If a tool needs to edit mod files, turn off **Share mod files between instances** in Settings.
  New installs then get their own copy in each instance.
- Configs, saves, screenshots and everything else in the instance are ordinary files you can edit
  freely.

If deleting a mod file also says "Access denied", report it with the instance's folder path and the
drive it's on.

### I deleted an instance by accident

Look under **Recently deleted**, below the cards on My Instances. Deleted instances stay there for 30 days, and you can restore them from there.

## Other games

### Agora can't find my game

Open Settings > Game support. Each game says whether it was **Found on this computer**. Agora searches Steam, GOG, the Epic Games Store and the Microsoft Store by itself. If your copy is somewhere else, such as another drive or a portable install, select **Add game folder** and pick the folder the game is in. After you move or reinstall the game, open Settings > (your game) > **Installs** and select **Rescan**.

### The game add-on won't install, or isn't official

The **More games** card installs Agora's official plugins. Read the prompt it shows before you approve it. If the download does not match what Agora expected, nothing is installed and the card says so.

A plugin you install from a file in Settings > Plugins works, but it does not get the official badge. Official means the signed catalog lists that exact version and the installed files still match.

A game plugin can build on another, shared plugin. Skyrim's builds on the Creation Engine base plugin, so both need to be installed; if the game does not appear, Settings > Plugins shows the game plugin as needing the missing one.

A game that is turned off in Settings > Game support is hidden from the game switcher. Its instances, bases and files are not touched.

### Agora cannot tell where an item's files go

Agora opens **Agora cannot tell where these files go** when the game has no rule for that archive's layout, and it lists the top-level entries it found. The item is not added, so nothing is misplaced. Check that you downloaded the mod's main file rather than a patch or an archive that only holds another archive, then add it again.

### A mod needs a framework, such as SKSE or Address Library

On the game's Browse, a mod that requires a framework stops before installing and names it. Use **Get (framework name)** to open the framework's own entry, install that first, then install the mod. **Install anyway** goes past the check, but the mod probably won't work until the framework is there.

The framework has to be installed in the instance, as content. A copy left in the real game folder by another mod manager is not detected. Launch checks keep naming a required framework that is missing, as a warning that does not stop the launch.

### Sort or Update masterlist fails

**Sort…** uses LOOT's masterlist when one has been saved. If the header says there is no masterlist yet, choose **Update masterlist** first; it downloads and checks the masterlist through GitHub, and a failed check leaves the saved copy alone. It needs **Let plugins reach the network** turned on in Settings > Plugins, which is off by default, and Lockdown Mode off in Settings > Privacy. Without a masterlist, Sort still puts each plugin below its masters, but not in LOOT's order.

If a plugin is locked or always loaded and cannot get below its master, the sort preview names it under **These cannot be fixed by sorting**. Unlock it and move it by hand.

### A tool won't run

Some tools, such as Nemesis and BodySlide, work on the real game folder, so Agora runs them on a copy of the game's **Data** folder that briefly takes its place. Agora shows the disk cost and asks first. Close Steam before agreeing, and do not start the game while the tool runs.

If Agora was closed during a run, the **Tools** tab says **A tool swap was interrupted** and offers **Put the Data folder back**. Do that before launching. Settings > (your game) > **Recovery** offers **Put the files back** for per-user files that are still swapped.

A tool whose card says **Out of date** should be rebuilt so the game reads output that matches your current mods.

### A mod says "Updates unknown"

The mod came from a page Agora does not download from, so Agora cannot check it for new versions. Visit the page yourself to see whether there is a newer file; download it and add it again to update the one in your library.

## Controllers

### My controller works in Agora but not in Minecraft

This is expected, because Minecraft has no built-in controller support. Agora offers to install a mod called Controlify when you launch with a controller in use. If you declined earlier, install it from Browse. It's available for Fabric, Quilt, and NeoForge.

### Agora doesn't respond to my controller

Press a button on the controller while the Agora window is focused (make sure to click into the app and that the controller is conencted via bluetooth, cable, or USB receiver). Agora only notices a controller once you use it. There is no setting to turn on, so if nothing happens at all, check that the controller works in another app. See [Playing with a controller](https://agoramc.com/docs/guides/controller-handheld).

## Agora itself

### Agora looks wrong or is hard to read

In Settings, under Appearance, use **Reset appearance** for colors and text, or **Reset layout** for the sidebar. Neither one touches your instances.

### Checking for a new version

Open Settings and go to **Software Updates**, then select **Check for Updates**. This also shows the version you're on, which is useful when asking for help. Portable copies update by hand: close Agora and replace the program file from a newer portable ZIP, keeping `portable.txt` and your `data` folder.

## Getting help

If you're still stuck, other people or AI assitants can help much faster with the details in hand.

1. For Minecraft, open Crash Doctor and select **Copy crash report**. It copies a report with your file paths and username removed. Nothing else is filtered, so give it a quick read for anything private, such as server addresses. For another game, include the game's own log or crash file and what the instance's **Overview > Launch checks** said.
2. Post it on the [Agora Discord](https://discord.gg/56tpsa2sTZ), along with your Agora version, what you were doing, and what you have already tried.
3. If you've found a bug in Agora itself that someone else could reproduce, please [open a bug report](https://github.com/agora-mc/Agora-Mod-Manager/issues/new?template=bug_report.yml).

You can also paste the report into an AI assistant, or [let one work with Agora directly](https://agoramc.com/docs/guides/ai-assistant). Whatever you paste goes to the company that runs it.

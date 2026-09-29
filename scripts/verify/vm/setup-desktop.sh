#!/usr/bin/env bash
# 给验证虚拟机装一层桌面（在**客机内**执行，可选）。
#
#   limactl shell strix -- scripts/verify/vm/setup-desktop.sh
#   limactl shell strix -- systemctl --user start strix-desktop
#   open vnc://127.0.0.1:5900          # macOS 自带的「屏幕共享」
#
# 为什么是 headless + VNC 而不是一个虚拟机窗口：
# Lima 的 `video.display` 只对 QEMU 后端有效，Apple 虚拟化框架这条路没有显示设备。
# 而且即便有，VZ 给 Linux 客机的 virtio-gpu 只有 2D，桌面一样是软件渲染。
# headless + VNC 反而更省：不连就一个像素都不画。
#
# 桌面**不是任何一条验收项的必要条件**。07 §2 的浏览器项写着「任一现代浏览器，
# SSH 隧道到 9700」——Lima 已经把客机端口转到宿主 127.0.0.1，用 macOS 上的浏览器
# 打开 http://127.0.0.1:9700 又快又准；`dbus-wedge-stress.sh` 要的那个
# 「正订阅 services.changed 的客户端」，宿主浏览器一样算数。
#
# 装这层的实际价值只有两条：Linux 侧浏览器的真实渲染（字体回退、深浅色跟随，
# 与 macOS 上的浏览器不是一回事），以及手上多一台随时能用的 Linux 图形机器。
set -euo pipefail

sudo dnf -y install \
    sway foot firefox waybar fuzzel wayvnc \
    fish neovim wl-clipboard \
    mesa-dri-drivers xorg-x11-server-Xwayland \
    google-noto-sans-cjk-fonts

# 字体：sway/foot 的配置里写的是 Maple Mono NF CN。客机挂载的只有仓库目录，
# 拷不到就是没有——那时 fontconfig 会退回上面装的 Noto CJK，中文照样显示，
# 只是不是你在 Ghostty 里看惯的那套。要一致就在**宿主**上跑一次：
#
#   tar -C ~/Library/Fonts -cf - $(cd ~/Library/Fonts && ls MapleMono-NF-CN-*) \
#     | limactl shell strix -- bash -lc \
#       'mkdir -p ~/.local/share/fonts/MapleMono && tar -C ~/.local/share/fonts/MapleMono -xf - && fc-cache -f'
mkdir -p ~/.local/share/fonts
fc-cache -f >/dev/null 2>&1 || true

mkdir -p ~/.config/sway ~/.config/foot
cat > ~/.config/sway/config <<'EOF'
# 验证虚拟机的桌面。目的单一：开一个浏览器看 StrixMaid 的页面，
# 外加一个真实的桌面会话（polkit 代理、用户总线）。
set $mod Mod4
set $term foot

# headless 后端造出来的输出就叫 HEADLESS-1；分辨率在这里改。
output HEADLESS-1 mode 1920x1200 position 0,0
output * bg #1a1b26 solid_color

font pango:Maple Mono NF CN 11
default_border pixel 2
gaps inner 6
smart_gaps on

client.focused          #7aa2f7 #7aa2f7 #1a1b26 #7aa2f7 #7aa2f7
client.unfocused        #292e42 #292e42 #a9b1d6 #292e42 #292e42
client.focused_inactive #414868 #414868 #a9b1d6 #414868 #414868

bindsym $mod+Return exec $term
bindsym $mod+d exec fuzzel
bindsym $mod+b exec firefox http://127.0.0.1:9700
bindsym $mod+q kill
bindsym $mod+Shift+e exit
bindsym $mod+f fullscreen toggle
bindsym $mod+Left focus left
bindsym $mod+Right focus right
bindsym $mod+Up focus up
bindsym $mod+Down focus down
floating_modifier $mod normal

bar {
    position top
    status_command while date +'%Y-%m-%d %H:%M'; do sleep 30; done
    colors {
        background #1a1b26
        statusline #a9b1d6
        separator  #414868
        focused_workspace  #7aa2f7 #7aa2f7 #1a1b26
        inactive_workspace #292e42 #292e42 #a9b1d6
    }
}

# 桌面一起来就开 VNC。
# `-o HEADLESS-1` 不能省：sway 里除了这个输出，还有一个内部的 `__i3` 空输出，
# wayvnc 默认抓到的是后者——画面会是一整片 sway 的默认灰，看着像桌面没起来。
# 只监听回环，Lima 会把它转到宿主的 127.0.0.1:5900。
exec wayvnc -o HEADLESS-1 127.0.0.1 5900
EOF

cat > ~/.config/foot/foot.ini <<'EOF'
font=Maple Mono NF CN:size=11
[colors-dark]
background=1a1b26
foreground=a9b1d6
EOF

mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/strix-desktop.service <<'EOF'
[Unit]
Description=StrixMaid 验证虚拟机的桌面（headless sway + wayvnc）
[Service]
Type=simple
# wlroots 的 headless 后端：没有真实显示设备也能起合成器，
# 画面通过 wayvnc 的 wlr-screencopy 送出去。
Environment=WLR_BACKENDS=headless
Environment=WLR_LIBINPUT_NO_DEVICES=1
Environment=XDG_CURRENT_DESKTOP=sway
Environment=XDG_SESSION_TYPE=wayland
ExecStart=/usr/bin/sway
Restart=no
[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
# 没有交互登录时用户实例也要活着（wayvnc 要常驻）。
sudo loginctl enable-linger "$USER"

cat <<'EOF'

装好了。起桌面：

    limactl shell strix -- systemctl --user start strix-desktop
    open vnc://127.0.0.1:5900

关掉（不用时一个像素都不画，内存也还回去）：

    limactl shell strix -- systemctl --user stop strix-desktop

快捷键：Super+Return 终端，Super+b 打开 StrixMaid 页面，Super+d 启动器。
EOF

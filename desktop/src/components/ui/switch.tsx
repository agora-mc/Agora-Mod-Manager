import * as React from "react"
import * as SwitchPrimitives from "@radix-ui/react-switch"

import { cn } from "@/lib/utils"

/**
 * Agora's on/off switch. Off reads as off whatever the theme: a neutral track (not `--input`,
 * which themes set to the accent), and only the on state takes the accent, with a soft glow.
 * The knob slides with a small spring, shows a check when on, and stretches while pressed.
 * Styles live in `index.css` (`.agora-switch`) so the app's Motion setting turns the
 * movement off along with every other transition.
 */
const Switch = React.forwardRef<
  React.ElementRef<typeof SwitchPrimitives.Root>,
  React.ComponentPropsWithoutRef<typeof SwitchPrimitives.Root> & { size?: "sm" | "md" }
>(({ className, size = "md", ...props }, ref) => (
  <SwitchPrimitives.Root
    className={cn("agora-switch peer", size === "sm" && "agora-switch--sm", className)}
    {...props}
    ref={ref}
  >
    <SwitchPrimitives.Thumb className="agora-switch__thumb">
      <svg className="agora-switch__check" viewBox="0 0 12 12" aria-hidden="true">
        <path d="M2.5 6.3 4.9 8.6 9.5 3.6" />
      </svg>
    </SwitchPrimitives.Thumb>
  </SwitchPrimitives.Root>
))
Switch.displayName = SwitchPrimitives.Root.displayName

export { Switch }

package wgtun

// TunnelState is one engine's tunnel as the Network tab shows it.
type TunnelState struct {
	State
	Provider      string `json:"provider"`
	ProviderLabel string `json:"provider_label"`
	ForwardedPort int    `json:"forwarded_port"`
	PortForward   string `json:"port_forward"`
	Note          string `json:"note,omitempty"`
}

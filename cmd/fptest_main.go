package main
import ("fmt"; v "github.com/Kheopsian/hydra/internal/version")
func main(){ fmt.Printf("fp=%q len=%d\n", v.PeerFingerprint(), len(v.PeerFingerprint())) }

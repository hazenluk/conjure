package lib

import (
	"encoding/hex"
	"fmt"
	"log"
	"net"
	"os"
	"sync"
	"time"

	"github.com/go-redis/redis"
	pb "github.com/refraction-networking/gotapdance/protobuf"
)

const DETECTOR_REG_CHANNEL string = "dark_decoy_map"

type RegistrationManager struct {
	registeredDecoys *RegisteredDecoys
	Logger           *log.Logger
	DDSelector       *DDIpSelector
}

func NewRegistrationManager() *RegistrationManager {
	logger := log.New(os.Stdout, "", log.Lmicroseconds)

	d, err := NewDDIpSelector()
	if err != nil {
		fmt.Errorf("Failed to create the DDIpSelector Object: %v\n", err)
		return nil
	}
	return &RegistrationManager{
		Logger:           logger,
		registeredDecoys: NewRegisteredDecoys(),
		DDSelector:       d,
	}
}

func (regManager *RegistrationManager) NewRegistration(c2s *pb.ClientToStation, conjureKeys *ConjureSharedKeys, flags [1]byte) (*DecoyRegistration, error) {

	darkDecoyAddr, err := regManager.DDSelector.Select(
		conjureKeys.DarkDecoySeed, uint(c2s.GetDecoyListGeneration()), c2s.GetV6Support())

	if err != nil {
		return nil, fmt.Errorf("Failed to select dark decoy IP address: %v", err)
	}

	reg := DecoyRegistration{
		DarkDecoy: darkDecoyAddr,
		keys:      conjureKeys,
		Covert:    c2s.GetCovertAddress(),
		Mask:      c2s.GetMaskedDecoyServerName(),
		Flags:     uint8(flags[0]),
	}

	return &reg, nil
}

func (regManager *RegistrationManager) AddRegistration(d *DecoyRegistration) {

	registerForDetector(d)

	darkDecoyAddr := d.DarkDecoy.String()
	regManager.registeredDecoys.register(darkDecoyAddr, d)
}

func (regManager *RegistrationManager) CheckRegistration(darkDecoyAddr *net.IP) *DecoyRegistration {
	return regManager.registeredDecoys.checkRegistration(darkDecoyAddr)
}

func (regManager *RegistrationManager) RemoveOldRegistrations() {
	regManager.registeredDecoys.removeOldRegistrations()
}

type DecoyRegistration struct {
	DarkDecoy    *net.IP
	keys         *ConjureSharedKeys
	Covert, Mask string
	Flags        uint8
}

// String -- Print a digest of the important identifying information for this registration.
//[TODO]{priority:soon} Find a way to add the client IP to this logging for now it is logged
// in the detector associating registrant IP with shared secret.
func (reg *DecoyRegistration) String() string {
	if reg == nil {
		return fmt.Sprintf("%v", reg.String())
	}

	reprStr := make([]byte, hex.EncodedLen(len(reg.keys.SharedSecret)))
	hex.Encode(reprStr, reg.keys.SharedSecret)
	digest := fmt.Sprintf("{phantom=%v, covert=%v, mask=%v, flags=0x%02x, Shared Secret:%s}",
		reg.DarkDecoy.String(), reg.Covert, reg.Mask, reg.Flags, reprStr)

	return digest
}

func (reg *DecoyRegistration) IDString() string {
	if reg == nil || reg.keys == nil {
		return "000000"
	}

	secret := make([]byte, hex.EncodedLen(len(reg.keys.SharedSecret)))
	n := hex.Encode(secret, reg.keys.SharedSecret)
	if n < 6 {
		return "000000"
	}
	return fmt.Sprintf("%s", secret[:6])
}

// PhantomIsLive - Test whether the phantom is live using
// 8 syns which returns syn-acks from 99% of sites within 1 second.
// see  ZMap: Fast Internet-wide Scanning  and Its Security Applications
// https://www.usenix.org/system/files/conference/usenixsecurity13/sec13-paper_durumeric.pdf
//
// return:	bool	true  - host is live
// 					false - host is not life
//			error	reason decision was made
func (reg *DecoyRegistration) PhantomIsLive() (bool, error) {
	if reg.DarkDecoy.To4() != nil {
		return phantomIsLive(reg.DarkDecoy.String() + ":443")
	}
	return phantomIsLive("[" + reg.DarkDecoy.String() + "]:443")
}

func phantomIsLive(address string) (bool, error) {
	width := 8
	dialError := make(chan error, width)

	testConnect := func() {
		conn, err := net.Dial("tcp", address)
		if err != nil {
			dialError <- err
			return
		}
		conn.Close()
		dialError <- nil
	}

	for i := 0; i < width; i++ {
		go testConnect()
	}

	timeout := 750 * time.Millisecond

	time.Sleep(timeout)

	// If any return errors or connect then return nil before deadline it is live
	select {
	case err := <-dialError:
		// fmt.Printf("Received: %v\n", err)
		if err != nil {
			return false, err
		}
		return true, nil
	default:
		return false, fmt.Errorf("Reached statistical timeout %v ms", timeout)
	}
}

type RegisteredDecoys struct {
	decoys         map[string]*DecoyRegistration
	decoysTimeouts []struct {
		decoy            string
		registrationTime time.Time
	}
	m sync.RWMutex
}

func NewRegisteredDecoys() *RegisteredDecoys {
	return &RegisteredDecoys{
		decoys: make(map[string]*DecoyRegistration),
	}
}

func (r *RegisteredDecoys) register(darkDecoyAddr string, d *DecoyRegistration) {
	r.m.Lock()
	if d != nil {
		r.decoys[darkDecoyAddr] = d
		r.decoysTimeouts = append(r.decoysTimeouts, struct {
			decoy            string
			registrationTime time.Time
		}{decoy: darkDecoyAddr, registrationTime: time.Now()})
	}
	r.m.Unlock()
}

func (r *RegisteredDecoys) checkRegistration(darkDecoyAddr *net.IP) *DecoyRegistration {
	darkDecoyAddrStatic := darkDecoyAddr.String()
	r.m.RLock()
	d := r.decoys[darkDecoyAddrStatic]
	r.m.RUnlock()
	return d
}

// TODO log registration expiration
func (r *RegisteredDecoys) removeOldRegistrations() {
	const timeout = -time.Minute * 5
	cutoff := time.Now().Add(timeout)
	idx := 0
	r.m.Lock()
	for idx < len(r.decoysTimeouts) {
		if cutoff.After(r.decoysTimeouts[idx].registrationTime) {
			break
		}
		delete(r.decoys, r.decoysTimeouts[idx].decoy)
		idx += 1
	}
	r.decoysTimeouts = r.decoysTimeouts[idx:]
	r.m.Unlock()
}

func registerForDetector(reg *DecoyRegistration) {
	client, err := getRedisClient()
	if err != nil {
		fmt.Printf("couldn't connect to redis")
	} else {
		client.Publish(DETECTOR_REG_CHANNEL, string(reg.DarkDecoy.To4()))
		client.Close()
	}
}

func getRedisClient() (*redis.Client, error) {
	var client *redis.Client
	client = redis.NewClient(&redis.Options{
		Addr:     "localhost:6379",
		Password: "",
		DB:       0,
		PoolSize: 10,
	})

	_, err := client.Ping().Result()
	if err != nil {
		return client, err
	}

	return client, err
}
